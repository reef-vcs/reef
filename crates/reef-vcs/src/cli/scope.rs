//! Which workspace and repository a VCS command means (R2.27, ADR 0034),
//! resolved once before the command's own call: `--workspace`, else
//! `--repo` (the repository and no workspace), else `REEF_WORKSPACE`,
//! else the workspace containing the current directory, else the
//! repository containing it. And the path rule: paths given are relative
//! to the current directory inside the scope, paths printed are
//! repository-relative, so a `next` runs verbatim from anywhere.

use std::path::{Component, Path, PathBuf};

use reef_api::v1::{self, GetWorkspaceRequest};
use reef_client::ClientError;
use reef_client::json::reverse_hex;
use reef_error::{Advice, ErrorKind};

use super::repo::missing;
use super::{CliError, Ctx, RepoArg};

/// `--workspace` and `--repo`, flattened into each command that takes a
/// scope.
#[derive(Clone, Debug, Default, clap::Args)]
pub(crate) struct ScopeArgs {
    /// A workspace id, unique prefix, or path inside it [default: the one you are in].
    #[arg(long, value_name = "WS")]
    pub(crate) workspace: Option<String>,
    /// The repository (id, id prefix, or path), and no workspace.
    #[arg(long, value_name = "REPO")]
    pub(crate) repo: Option<String>,
}

/// `--at-op` and `--no-snapshot`, for the commands that read.
#[derive(Clone, Debug, Default, clap::Args)]
pub(crate) struct ReadArgs {
    /// Read at this operation (id or unique prefix) instead of now.
    #[arg(long = "at-op", visible_alias = "at-operation", value_name = "OP")]
    pub(crate) at_op: Option<String>,
    /// Read the last snapshot instead of snapshotting the workspace first.
    #[arg(long)]
    pub(crate) no_snapshot: bool,
}

impl ReadArgs {
    /// The request's `at_operation`.
    pub(crate) fn at_operation(&self) -> String {
        self.at_op.clone().unwrap_or_default()
    }
}

/// What a command means: a repository, and a workspace in it or none.
#[derive(Clone, Debug)]
pub(crate) struct Scope {
    /// The repository's full id.
    pub(crate) repo_id: String,
    /// The workspace, as looked up (no snapshot); `None` in repository
    /// scope.
    pub(crate) workspace: Option<v1::Workspace>,
    /// The workspace's directory, or the repository's root: what given
    /// paths are relative to.
    pub(crate) dir: PathBuf,
    /// Whether `--workspace` or `--repo` chose it, so relative paths are
    /// repository-relative as given.
    pub(crate) from_flags: bool,
}

impl Scope {
    /// The workspace's full id, empty in repository scope: the requests'
    /// `workspace` field.
    pub(crate) fn workspace_id(&self) -> String {
        self.workspace
            .as_ref()
            .map(|ws| ws.id.clone())
            .unwrap_or_default()
    }

    /// The workspace's full id; `None` in repository scope.
    pub(crate) fn workspace_ref(&self) -> Option<&str> {
        self.workspace.as_ref().map(|ws| ws.id.as_str())
    }

    /// The flags that name this scope in a `next`: `--workspace <id>`, or
    /// `--repo <id>` in repository scope.
    pub(crate) fn flags(&self) -> Vec<String> {
        match &self.workspace {
            Some(ws) => vec!["--workspace".to_owned(), ws.id.clone()],
            None => vec!["--repo".to_owned(), self.repo_id.clone()],
        }
    }
}

/// The scope `args` and the environment name.
pub(crate) async fn resolve(ctx: &Ctx, args: &ScopeArgs) -> Result<Scope, CliError> {
    if let Some(workspace) = &args.workspace {
        let repo = match &args.repo {
            Some(repo) => repo_text(ctx, &RepoArg::parse(repo)?).await?,
            None => String::new(),
        };
        let ws = lookup(ctx, repo, workspace_arg(workspace)?).await?;
        return Ok(in_workspace(ws, true));
    }
    if let Some(repo) = &args.repo {
        return repository(ctx, &RepoArg::parse(repo)?).await;
    }
    implicit(ctx).await
}

/// The repository alone, for `op log` and `op show`: `--repo`, else the
/// repository of the workspace or directory [`resolve`] would find.
pub(crate) async fn resolve_repo_only(ctx: &Ctx, repo: Option<&str>) -> Result<Scope, CliError> {
    let scope = match repo {
        Some(repo) => repository(ctx, &RepoArg::parse(repo)?).await?,
        None => implicit(ctx).await?,
    };
    Ok(Scope {
        workspace: None,
        ..scope
    })
}

/// The workspace `REEF_WORKSPACE` or the current directory names, without
/// falling back to the repository: `ws snapshot` with no argument. `None`
/// outside every workspace.
pub(crate) async fn implicit_workspace(ctx: &Ctx) -> Result<Option<v1::Workspace>, CliError> {
    if let Some(id) = env_workspace() {
        return lookup(ctx, String::new(), id).await.map(Some);
    }
    let cwd = current_dir()?;
    match lookup(ctx, String::new(), cwd.display().to_string()).await {
        Ok(ws) => Ok(Some(ws)),
        Err(CliError::Daemon(error)) if is_workspace_not_found(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Steps 3 and 4: `REEF_WORKSPACE`, else the current directory's workspace,
/// else its repository.
async fn implicit(ctx: &Ctx) -> Result<Scope, CliError> {
    if let Some(id) = env_workspace() {
        let ws = lookup(ctx, String::new(), id).await?;
        return Ok(in_workspace(ws, false));
    }
    let cwd = current_dir()?;
    match lookup(ctx, String::new(), cwd.display().to_string()).await {
        Ok(ws) => Ok(in_workspace(ws, false)),
        Err(CliError::Daemon(error)) if is_workspace_not_found(&error) => {
            registered(ctx, &RepoArg::Path(cwd), false).await
        }
        Err(error) => Err(error),
    }
}

/// `REEF_WORKSPACE`, when set and not blank, as `here` reads it.
fn env_workspace() -> Option<String> {
    std::env::var("REEF_WORKSPACE")
        .ok()
        .filter(|id| !id.trim().is_empty())
}

fn in_workspace(ws: v1::Workspace, from_flags: bool) -> Scope {
    Scope {
        repo_id: ws.repo_id.clone(),
        dir: PathBuf::from(&ws.path),
        workspace: Some(ws),
        from_flags,
    }
}

/// `GetWorkspace` without a snapshot or a disk walk.
async fn lookup(ctx: &Ctx, repo: String, workspace: String) -> Result<v1::Workspace, CliError> {
    let response = ctx
        .call(ctx.client.get_workspace(GetWorkspaceRequest {
            repo,
            workspace,
            snapshot_first: false,
            skip_disk_usage: true,
        }))
        .await?;
    response.workspace.ok_or_else(|| missing("workspace"))
}

/// `--repo`'s repository; see [`in_workspace_repo`].
async fn repository(ctx: &Ctx, repo: &RepoArg) -> Result<Scope, CliError> {
    let repo = in_workspace_repo(ctx, repo)
        .await?
        .unwrap_or_else(|| repo.clone());
    registered(ctx, &repo, true).await
}

/// `--repo`'s text as a request's `repo` field, alongside `--workspace`.
async fn repo_text(ctx: &Ctx, repo: &RepoArg) -> Result<String, CliError> {
    let repo = in_workspace_repo(ctx, repo)
        .await?
        .unwrap_or_else(|| repo.clone());
    Ok(repo.text())
}

/// The repository of the workspace a `--repo` path is inside, if it is
/// inside one: workspaces live outside their repository's root, so the
/// path alone names no repository, and registering it would fail.
async fn in_workspace_repo(ctx: &Ctx, repo: &RepoArg) -> Result<Option<RepoArg>, CliError> {
    let RepoArg::Path(path) = repo else {
        return Ok(None);
    };
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
    match lookup(ctx, String::new(), canonical.display().to_string()).await {
        Ok(ws) => Ok(Some(RepoArg::Named(ws.repo_id))),
        Err(CliError::Daemon(error)) if is_workspace_not_found(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Repository scope: `GetRepository`, registering a path in no registered
/// repository as every other command does (R1.25).
async fn registered(ctx: &Ctx, repo: &RepoArg, from_flags: bool) -> Result<Scope, CliError> {
    let response = ctx
        .call_with_repo(repo, |repo| async move {
            ctx.client.get_repository(&repo).await
        })
        .await?;
    let repo = response.repository.ok_or_else(|| missing("repository"))?;
    Ok(Scope {
        repo_id: repo.id,
        workspace: None,
        dir: PathBuf::from(repo.root),
        from_flags,
    })
}

/// A workspace as a command line names it: a path that exists, canonical
/// and absolute (the daemon reads only absolute paths as paths), else an id
/// or unique prefix as given.
pub(crate) fn workspace_arg(text: &str) -> Result<String, CliError> {
    let path = Path::new(text);
    if text.is_empty() || !path.exists() {
        return Ok(text.to_owned());
    }
    std::fs::canonicalize(path)
        .map(|path| path.display().to_string())
        .map_err(|source| CliError::Io {
            op: "resolve",
            path: path.to_owned(),
            source,
        })
}

/// Whether the daemon found no workspace: reason `WORKSPACE_NOT_FOUND`
/// (R2.05), which outside every workspace is an answer, not an error.
pub(crate) fn is_workspace_not_found(error: &ClientError) -> bool {
    error.reason() == reef_api::error::WORKSPACE_NOT_FOUND
}

/// The current directory, canonical like the paths the daemon records.
pub(crate) fn current_dir() -> Result<PathBuf, CliError> {
    std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|source| CliError::Io {
            op: "read the current directory",
            path: PathBuf::from("."),
            source,
        })
}

/// The directory relative paths are relative to: the current directory
/// when the scope came from the environment and the current directory is
/// inside it; `None` when relative paths are repository-relative as given.
fn relative_base(scope: &Scope) -> Result<Option<PathBuf>, CliError> {
    if scope.from_flags {
        return Ok(None);
    }
    let cwd = current_dir()?;
    Ok(cwd.starts_with(&scope.dir).then_some(cwd))
}

/// The paths a command was given, repository-relative.
pub(crate) fn given_paths(scope: &Scope, given: &[String]) -> Result<Vec<String>, CliError> {
    let base = relative_base(scope)?;
    given
        .iter()
        .map(|path| given_path(scope, base.as_deref(), path))
        .collect()
}

/// `file list`'s path when none is given: the current directory inside the
/// scope, else the root.
pub(crate) fn default_list_path(scope: &Scope) -> Result<String, CliError> {
    match relative_base(scope)? {
        Some(cwd) => Ok(relative_to(&scope.dir, &cwd).unwrap_or_default()),
        None => Ok(String::new()),
    }
}

fn given_path(scope: &Scope, base: Option<&Path>, given: &str) -> Result<String, CliError> {
    let path = Path::new(given);
    let relative = if path.is_absolute() {
        relative_to(&scope.dir, path).or_else(|| through_symlinked_ancestor(&scope.dir, path))
    } else {
        relative_to(&scope.dir, &base.unwrap_or(&scope.dir).join(path))
    };
    relative.ok_or_else(|| CliError::Precheck {
        kind: ErrorKind::InvalidArgument,
        reason: reef_api::error::INVALID_PATH,
        message: format!("{given} is outside {}", scope.dir.display()),
        advice: Advice::hint(PATH_HINT),
    })
}

/// How paths are given, for `INVALID_PATH`.
pub(crate) const PATH_HINT: &str = "Paths are relative to the current directory inside the workspace or repository, or absolute inside it, and never leave it.";

/// `path`, absolute, relative to `dir` (directly, or through a symlinked
/// ancestor that is `dir`); `None` when it is outside. `reef mcp`'s
/// absolute paths (R2.13).
pub(crate) fn inside(dir: &Path, path: &Path) -> Option<String> {
    relative_to(dir, path).or_else(|| through_symlinked_ancestor(dir, path))
}

/// `path`, absolute and outside `dir` as written, relative to `dir` when
/// one of its ancestors is `dir` through a symlink (macOS's `/tmp` is
/// `/private/tmp`, and the daemon records canonical directories). Only
/// that ancestor is resolved: components under `dir` are read through the
/// tree, never through a symlink on disk, as the API reads them.
fn through_symlinked_ancestor(dir: &Path, path: &Path) -> Option<String> {
    let normal = normalize(path);
    normal
        .ancestors()
        .find(|ancestor| std::fs::canonicalize(ancestor).is_ok_and(|real| real == dir))
        .and_then(|ancestor| relative_to(ancestor, &normal))
}

/// `path`, absolute, relative to `dir` and `/`-separated after resolving
/// `.` and `..` by components, without the disk; `None` when it leaves
/// `dir`. `dir` itself is the empty path.
fn relative_to(dir: &Path, path: &Path) -> Option<String> {
    let normal = normalize(path);
    let rest = normal.strip_prefix(dir).ok()?;
    let parts: Vec<String> = rest
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(parts.join("/"))
}

/// `.` and `..` resolved lexically; `..` at the root stays at the root.
fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

/// `reef log --workspace <id> -r 'change_id(<reverse hex>)'`: the commits
/// of a divergent workspace's change, for the note after a write and for
/// `here` (issue #80).
pub(crate) fn divergent_log(workspace_id: &str, change_id: &str) -> Vec<String> {
    vec![
        "reef".to_owned(),
        "log".to_owned(),
        "--workspace".to_owned(),
        workspace_id.to_owned(),
        "-r".to_owned(),
        format!("change_id({})", reverse_hex(change_id)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(dir: &str, from_flags: bool) -> Scope {
        Scope {
            repo_id: "aaaa000000000001".into(),
            workspace: None,
            dir: PathBuf::from(dir),
            from_flags,
        }
    }

    fn given(scope: &Scope, cwd: Option<&str>, path: &str) -> Result<String, String> {
        given_path(scope, cwd.map(Path::new), path).map_err(|error| error.to_string())
    }

    /// R2.27 test 22: the path rule over directories that need not exist.
    #[test]
    fn paths_are_relative_in_and_repository_relative_out() {
        let ws = scope("/ws/one", false);
        // Relative to the current directory inside the workspace.
        assert_eq!(given(&ws, Some("/ws/one/src"), "a.rs").unwrap(), "src/a.rs");
        assert_eq!(given(&ws, Some("/ws/one/src"), "../b.rs").unwrap(), "b.rs");
        assert_eq!(
            given(&ws, Some("/ws/one/src"), "./x/../y/z.rs").unwrap(),
            "src/y/z.rs"
        );
        assert_eq!(given(&ws, Some("/ws/one"), ".").unwrap(), "");
        // Leaving the workspace is refused.
        let error = given(&ws, Some("/ws/one/src"), "../../x").unwrap_err();
        assert!(error.contains("../../x is outside /ws/one"), "{error}");
        assert!(given(&ws, Some("/ws/one"), "../one2/a").is_err());
        // Absolute inside and outside (these do not exist, so they are not
        // canonicalized).
        assert_eq!(
            given(&ws, Some("/ws/one/src"), "/ws/one/lib/c.rs").unwrap(),
            "lib/c.rs"
        );
        assert!(given(&ws, Some("/ws/one/src"), "/elsewhere/c.rs").is_err());
        // The current directory outside the workspace, or flags given:
        // repository-relative as given.
        assert_eq!(given(&ws, None, "src/a.rs").unwrap(), "src/a.rs");
        let flagged = scope("/ws/one", true);
        assert_eq!(given(&flagged, None, "a.rs").unwrap(), "a.rs");
    }

    /// Review finding 1 and 2: only an ancestor that is the directory
    /// through a symlink is resolved; symlinks under it are kept, and `..`
    /// in a part that does not exist is resolved first.
    #[test]
    fn absolute_paths_resolve_only_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().canonicalize().unwrap().join("ws");
        std::fs::create_dir_all(real.join("v2")).unwrap();
        std::fs::write(real.join("README.md"), "r\n").unwrap();
        std::os::unix::fs::symlink("README.md", real.join("link")).unwrap();
        std::os::unix::fs::symlink("v2", real.join("latest")).unwrap();
        std::os::unix::fs::symlink("/etc", real.join("out")).unwrap();
        let alias = tmp.path().canonicalize().unwrap().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let ws = scope(real.to_str().unwrap(), false);
        let at = |dir: &Path, rest: &str| dir.join(rest).display().to_string();
        for dir in [&real, &alias] {
            assert_eq!(given(&ws, None, &at(dir, "link")).unwrap(), "link");
            assert_eq!(
                given(&ws, None, &at(dir, "latest/a.txt")).unwrap(),
                "latest/a.txt"
            );
            assert_eq!(given(&ws, None, &at(dir, "out")).unwrap(), "out");
            assert_eq!(given(&ws, None, &at(dir, "gone/../x")).unwrap(), "x");
        }
        assert!(given(&ws, None, &at(&alias, "../elsewhere")).is_err());
    }

    #[test]
    fn relative_to_compares_whole_components() {
        assert_eq!(
            relative_to(Path::new("/ws/one"), Path::new("/ws/one/a/b")).as_deref(),
            Some("a/b")
        );
        assert_eq!(
            relative_to(Path::new("/ws/one"), Path::new("/ws/one2/a")),
            None
        );
        assert_eq!(
            relative_to(Path::new("/ws/one"), Path::new("/ws/one")).as_deref(),
            Some("")
        );
    }

    #[test]
    fn scope_flags_name_the_workspace_else_the_repository() {
        let mut ws = scope("/ws/one", false);
        assert_eq!(ws.flags(), ["--repo", "aaaa000000000001"]);
        ws.workspace = Some(v1::Workspace {
            id: "bbbb000000000001".into(),
            ..v1::Workspace::default()
        });
        assert_eq!(ws.flags(), ["--workspace", "bbbb000000000001"]);
        assert_eq!(ws.workspace_id(), "bbbb000000000001");
    }

    #[test]
    fn divergent_log_names_the_change_in_reverse_hex() {
        assert_eq!(
            divergent_log("bbbb000000000001", "0123"),
            [
                "reef",
                "log",
                "--workspace",
                "bbbb000000000001",
                "-r",
                "change_id(zyxw)"
            ]
        );
    }
}
