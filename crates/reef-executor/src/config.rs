//! `executor.toml` and `reef-checks.toml` (ADR 0060 decision 2).
//!
//! Both are parsed strictly: an unknown key is an error naming its line.
//! A **check** is one declared command: a kind, the targets it accepts, the
//! argv for the whole check (`run`) and for one target (`run_target`), and
//! what surrounds the run. The executor's own checks may also ask for a
//! cache and any network level; a repository's may not
//! ([`RepoChecks`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::Error;

/// The most checks a repository file may declare.
pub const MAX_REPOSITORY_CHECKS: usize = 64;

/// The default target pattern: no spaces, quotes, `$`, `;`, backticks or
/// control characters, 1 to 512 bytes.
pub const DEFAULT_TARGET_PATTERN: &str = r"^[A-Za-z0-9._/:@+=-]{1,512}$";

/// Program names that are shells: `{target}` in the script after their
/// `-c` is refused.
const SHELLS: &[&str] = &[
    "sh", "bash", "dash", "zsh", "ksh", "fish", "mksh", "ash", "busybox",
];

/// How much network a check gets, in increasing order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// None: on Linux a namespace with only its own loopback; on macOS no
    /// sockets at all.
    #[default]
    None,
    /// The host's loopback on macOS; the same as `none` on Linux.
    Loopback,
    /// The host's network.
    Full,
}

impl Network {
    /// The level's name in the files and the annotations.
    pub fn as_str(self) -> &'static str {
        match self {
            Network::None => "none",
            Network::Loopback => "loopback",
            Network::Full => "full",
        }
    }
}

/// Whether the tree's `reef-checks.toml` is read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RepositoryChecks {
    /// Read it; the executor's own checks still come first.
    #[default]
    Allow,
    /// Run only the executor's own checks.
    Deny,
}

/// Which hash checks run after the tree is laid out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verify {
    /// The listing, each blob, and the laid-out disk.
    #[default]
    Disk,
    /// The listing and each blob only.
    Listing,
}

impl Verify {
    /// The name in the annotations.
    pub fn as_str(self) -> &'static str {
        match self {
            Verify::Disk => "disk",
            Verify::Listing => "listing",
        }
    }
}

/// The sandbox to run checks in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxKind {
    /// Seatbelt on macOS, bubblewrap on Linux, refused elsewhere.
    #[default]
    Auto,
    /// `sandbox-exec`.
    Seatbelt,
    /// `bwrap`.
    Bubblewrap,
    /// No sandbox: checks run as plain child processes. For executors that
    /// are already isolated.
    None,
}

/// `executor.toml`, as parsed and checked.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where it was read from; `None` for the defaults.
    pub path: Option<PathBuf>,
    /// `[executor]`.
    pub executor: ExecutorSection,
    /// `[repository]`.
    pub repository: RepositorySection,
    /// `[declarations]`'s target pattern, compiled.
    pub target_pattern: regex::Regex,
    /// `[materialize]`.
    pub materialize: MaterializeSection,
    /// `[work] dir`, absolute; the platform cache directory's
    /// `reef/executor` by default.
    pub work_dir: PathBuf,
    /// `[sandbox]`.
    pub sandbox: SandboxSection,
    /// `[run]`.
    pub run: RunSection,
    /// The executor's own checks.
    pub checks: Vec<Check>,
}

/// `[executor]`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExecutorSection {
    /// The executor's name in `ExecutorInfo`; `reef-executor@<host>` when
    /// empty.
    pub name: String,
    /// Labels added to `os=`, `arch=` and `sandbox=`.
    pub labels: Vec<String>,
    /// Only these repositories; empty for all.
    pub repos: Vec<String>,
    /// The session keys' identity; empty for the default.
    pub identity: String,
    /// Sign each job's evidence with a keyless (Sigstore) key (R3.42):
    /// Fulcio certifies it against the job's OIDC token, and its identity is
    /// whichever accepts the token's issuer and subject; `identity` must be
    /// empty. Default false.
    pub keyless: bool,
    /// The claim's lease: 15 seconds to 10 minutes.
    #[serde(with = "humantime_serde")]
    pub lease: Duration,
}

impl Default for ExecutorSection {
    fn default() -> Self {
        ExecutorSection {
            name: String::new(),
            labels: Vec::new(),
            repos: Vec::new(),
            identity: String::new(),
            keyless: false,
            lease: Duration::from_mins(1),
        }
    }
}

/// `[repository]`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct RepositorySection {
    /// Whether the tree's `reef-checks.toml` is read.
    pub checks: RepositoryChecks,
    /// The kinds claimed for repository-declared checks.
    pub kinds: Vec<String>,
}

impl Default for RepositorySection {
    fn default() -> Self {
        RepositorySection {
            checks: RepositoryChecks::Allow,
            kinds: vec!["build".into(), "lint".into(), "test".into()],
        }
    }
}

/// `[materialize]`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct MaterializeSection {
    /// Which checks run after the layout.
    pub verify: Verify,
    /// The blob cache's bound, in bytes.
    #[serde(deserialize_with = "size")]
    pub cache_size: u64,
}

impl Default for MaterializeSection {
    fn default() -> Self {
        MaterializeSection {
            verify: Verify::Disk,
            cache_size: 4 << 30,
        }
    }
}

/// `[sandbox]`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct SandboxSection {
    /// The sandbox.
    pub kind: SandboxKind,
    /// `bwrap`'s path; empty for `bwrap` on `PATH`.
    pub bwrap: String,
    /// Paths readable inside the hidden home (toolchains), `~` expanded.
    pub read: Vec<String>,
    /// Variables copied from the executor's environment into every
    /// check's.
    pub pass_env: Vec<String>,
    /// Variables set in every check's environment, `~` expanded at the
    /// start of a value; a `pass_env` variable the executor has wins.
    pub env: BTreeMap<String, String>,
    /// Paths hidden from every check even under a `read` path, `~`
    /// expanded: credentials kept beside a toolchain.
    pub deny: Vec<String>,
    /// The most network a repository's check may have.
    pub max_repository_network: Network,
}

impl Default for SandboxSection {
    fn default() -> Self {
        SandboxSection {
            kind: SandboxKind::Auto,
            bwrap: String::new(),
            read: vec!["~/.rustup".into(), "~/.cargo".into()],
            pass_env: vec!["RUSTUP_HOME".into(), "CARGO_HOME".into()],
            // Where rustup and cargo keep their toolchains when the
            // executor's environment does not say: the check's HOME is
            // its own.
            env: BTreeMap::from([
                ("CARGO_HOME".to_owned(), "~/.cargo".to_owned()),
                ("RUSTUP_HOME".to_owned(), "~/.rustup".to_owned()),
            ]),
            // cargo's registry tokens (`cargo login`).
            deny: vec![
                "~/.cargo/credentials".into(),
                "~/.cargo/credentials.toml".into(),
            ],
            max_repository_network: Network::None,
        }
    }
}

/// `[run]`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct RunSection {
    /// The most of a check's output kept, in bytes.
    #[serde(deserialize_with = "size")]
    pub max_log: u64,
    /// The largest declared artifact uploaded, in bytes.
    #[serde(deserialize_with = "size")]
    pub max_artifact: u64,
    /// Between `SIGTERM` and `SIGKILL`.
    #[serde(with = "humantime_serde")]
    pub kill_grace: Duration,
}

impl Default for RunSection {
    fn default() -> Self {
        RunSection {
            max_log: 16 << 20,
            max_artifact: 64 << 20,
            kill_grace: Duration::from_secs(10),
        }
    }
}

/// The file as written, before checks.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawConfig {
    executor: ExecutorSection,
    repository: RepositorySection,
    declarations: RawDeclarations,
    materialize: MaterializeSection,
    work: RawWork,
    sandbox: SandboxSection,
    run: RunSection,
    check: Vec<RawCheck>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawDeclarations {
    target_pattern: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawWork {
    dir: String,
}

/// A `[[check]]` table as written. A repository's has no `cache`
/// ([`RawRepoCheck`]).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCheck {
    name: String,
    kind: String,
    #[serde(default)]
    targets: Vec<String>,
    run: Option<Vec<String>>,
    run_target: Option<Vec<String>>,
    #[serde(default, with = "humantime_serde")]
    timeout: Option<Duration>,
    #[serde(default)]
    network: Network,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    error_exit: Vec<i32>,
    #[serde(default)]
    cache: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepoCheck {
    name: String,
    kind: String,
    #[serde(default)]
    targets: Vec<String>,
    run: Option<Vec<String>>,
    run_target: Option<Vec<String>>,
    #[serde(default, with = "humantime_serde")]
    timeout: Option<Duration>,
    #[serde(default)]
    network: Network,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    error_exit: Vec<i32>,
}

impl From<RawRepoCheck> for RawCheck {
    fn from(c: RawRepoCheck) -> Self {
        RawCheck {
            name: c.name,
            kind: c.kind,
            targets: c.targets,
            run: c.run,
            run_target: c.run_target,
            timeout: c.timeout,
            network: c.network,
            env: c.env,
            artifacts: c.artifacts,
            error_exit: c.error_exit,
            cache: false,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawRepoFile {
    check: Vec<RawRepoCheck>,
}

/// Where a check was declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// `executor.toml`.
    Executor,
    /// `reef-checks.toml` at the job's tree.
    Repository,
}

impl Source {
    /// The name in the annotations.
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Executor => "executor",
            Source::Repository => "repository",
        }
    }
}

/// One piece of an argv element or an `env` value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    /// Literal text.
    Text(String),
    /// `{target}`.
    Target,
    /// `{cache}`: the check's cache directory.
    Cache,
}

/// An argv element or `env` value with its placeholders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template(pub Vec<Piece>);

impl Template {
    /// The text with `target` and `cache` substituted.
    pub fn render(&self, target: Option<&str>, cache: Option<&Path>) -> String {
        let mut out = String::new();
        for piece in &self.0 {
            match piece {
                Piece::Text(text) => out.push_str(text),
                Piece::Target => out.push_str(target.unwrap_or_default()),
                Piece::Cache => {
                    if let Some(cache) = cache {
                        out.push_str(&cache.to_string_lossy());
                    }
                }
            }
        }
        out
    }

    fn has(&self, wanted: &Piece) -> bool {
        self.0.iter().any(|piece| piece == wanted)
    }
}

/// A declared check, checked.
#[derive(Clone, Debug)]
pub struct Check {
    /// Unique in its file.
    pub name: String,
    /// The evidence kind it produces.
    pub kind: String,
    /// The target globs it accepts.
    pub targets: globset::GlobSet,
    /// The globs as written.
    pub target_globs: Vec<String>,
    /// The argv for the whole check.
    pub run: Option<Vec<Template>>,
    /// The argv for one target.
    pub run_target: Option<Vec<Template>>,
    /// At most this long; the job's remaining wall time when `None`.
    pub timeout: Option<Duration>,
    /// The network it asks for.
    pub network: Network,
    /// Literal variables, `{cache}` substituted.
    pub env: Vec<(String, Template)>,
    /// Tree-relative files uploaded after the run.
    pub artifacts: Vec<String>,
    /// Exit codes that mean "no verdict".
    pub error_exit: Vec<i32>,
    /// Whether it has a persistent cache (executor checks only).
    pub cache: bool,
    /// Where it was declared.
    pub source: Source,
    /// SHA-256 of its canonical declaration, in the annotations.
    pub declaration: String,
}

impl Config {
    /// The defaults: repository checks in the default sandbox.
    pub fn defaults() -> Result<Config, Error> {
        Config::from_raw(RawConfig::default(), None)
    }

    /// Reads `path`, which must exist.
    pub fn load(path: &Path) -> Result<Config, Error> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
            context: format!("reading {}", path.display()),
            source,
        })?;
        Config::parse(&text, Some(path))
    }

    /// The configuration `explicit` names, else `REEF_EXECUTOR_CONFIG`'s,
    /// else `<state>/executor.toml` when it exists, else the defaults.
    pub fn locate(explicit: Option<&Path>, state_dir: &Path) -> Result<Config, Error> {
        if let Some(path) = explicit {
            return Config::load(path);
        }
        if let Some(path) = std::env::var_os(crate::CONFIG_ENV).filter(|v| !v.is_empty()) {
            return Config::load(Path::new(&path));
        }
        let default = state_dir.join(crate::CONFIG_FILE);
        if default.exists() {
            Config::load(&default)
        } else {
            Config::defaults()
        }
    }

    /// Parses `text`, read from `path`.
    pub fn parse(text: &str, path: Option<&Path>) -> Result<Config, Error> {
        let raw: RawConfig = toml::from_str(text).map_err(|error| Error::Config {
            message: format!("{}: {error}", display(path)),
        })?;
        Config::from_raw(raw, path)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one check per section, in file order"
    )]
    fn from_raw(raw: RawConfig, path: Option<&Path>) -> Result<Config, Error> {
        let fail = |message: String| Error::Config {
            message: format!("{}: {message}", display(path)),
        };
        if raw.executor.keyless && !raw.executor.identity.is_empty() {
            return Err(fail(format!(
                "[executor] keyless = true with identity = {:?}: a keyless key's identity is the one whose OIDC match accepts its token; leave identity empty",
                raw.executor.identity
            )));
        }
        let lease = raw.executor.lease;
        if lease < Duration::from_secs(15) || lease > Duration::from_mins(10) {
            return Err(fail(format!(
                "[executor] lease = {}: a lease is 15s to 10m",
                humantime::format_duration(lease)
            )));
        }
        if !raw.executor.name.is_empty() && !visible(&raw.executor.name, 128) {
            return Err(fail(
                "[executor] name: 1 to 128 visible ASCII characters".into(),
            ));
        }
        for label in &raw.executor.labels {
            if !label.contains('=') || !visible(label, 128) {
                return Err(fail(format!(
                    "[executor] labels: {label:?} is not `key=value` in visible ASCII"
                )));
            }
        }
        for kind in &raw.repository.kinds {
            if !reef_evidence::verify::is_kind(kind) {
                return Err(fail(format!("[repository] kinds: {kind:?} is not a kind")));
            }
        }
        let pattern = raw
            .declarations
            .target_pattern
            .as_deref()
            .unwrap_or(DEFAULT_TARGET_PATTERN);
        let target_pattern = regex::Regex::new(pattern)
            .map_err(|error| fail(format!("[declarations] target_pattern: {error}")))?;
        for name in &raw.sandbox.pass_env {
            if name.starts_with("REEF_") {
                return Err(fail(format!(
                    "[sandbox] pass_env: {name} is Reef's; no REEF_ variable reaches a check"
                )));
            }
            if !env_name(name) {
                return Err(fail(format!(
                    "[sandbox] pass_env: {name:?} is not a variable name"
                )));
            }
        }
        for (name, value) in &raw.sandbox.env {
            if name.starts_with("REEF_") || !env_name(name) {
                return Err(fail(format!(
                    "[sandbox] env: {name:?} is not a variable name a check may get"
                )));
            }
            if value.contains('\0') {
                return Err(fail(format!("[sandbox] env: {name}'s value has a NUL")));
            }
        }
        let home = home_dir();
        for read in &raw.sandbox.read {
            let expanded = expand(read, home.as_deref());
            if !expanded.is_absolute() {
                return Err(fail(format!(
                    "[sandbox] read: {read:?} is not absolute (or ~/…)"
                )));
            }
        }
        let work_dir = if raw.work.dir.is_empty() {
            default_work_dir()?
        } else {
            let dir = expand(&raw.work.dir, home.as_deref());
            if !dir.is_absolute() {
                return Err(fail(format!(
                    "[work] dir: {:?} is not absolute",
                    raw.work.dir
                )));
            }
            dir
        };
        if raw.run.kill_grace > Duration::from_mins(1) {
            return Err(fail("[run] kill_grace: at most 1m".into()));
        }
        let mut checks = Vec::new();
        for (index, raw_check) in raw.check.into_iter().enumerate() {
            let check = Check::from_raw(raw_check, Source::Executor)
                .map_err(|message| fail(format!("[[check]] #{}: {message}", index + 1)))?;
            if checks.iter().any(|c: &Check| c.name == check.name) {
                return Err(fail(format!(
                    "[[check]] #{}: a second check named {:?}",
                    index + 1,
                    check.name
                )));
            }
            checks.push(check);
        }
        Ok(Config {
            path: path.map(Path::to_owned),
            executor: raw.executor,
            repository: raw.repository,
            target_pattern,
            materialize: raw.materialize,
            work_dir,
            sandbox: raw.sandbox,
            run: raw.run,
            checks,
        })
    }

    /// `[sandbox] env`, `~` expanded.
    pub fn sandbox_env(&self) -> Vec<(String, String)> {
        let home = home_dir();
        self.sandbox
            .env
            .iter()
            .map(|(name, value)| {
                let value = if value == "~" || value.starts_with("~/") {
                    expand(value, home.as_deref())
                        .to_string_lossy()
                        .into_owned()
                } else {
                    value.clone()
                };
                (name.clone(), value)
            })
            .collect()
    }

    /// The `[sandbox] deny` paths, `~` expanded.
    pub fn deny_paths(&self) -> Vec<PathBuf> {
        let home = home_dir();
        self.sandbox
            .deny
            .iter()
            .map(|deny| expand(deny, home.as_deref()))
            .collect()
    }

    /// The `[sandbox] read` paths, `~` expanded.
    pub fn read_paths(&self) -> Vec<PathBuf> {
        let home = home_dir();
        self.sandbox
            .read
            .iter()
            .map(|read| expand(read, home.as_deref()))
            .collect()
    }

    /// The kinds to claim: the executor's own checks', and
    /// `[repository] kinds` when repository checks are allowed. Sorted,
    /// without duplicates.
    pub fn kinds(&self) -> Vec<String> {
        let mut kinds: Vec<String> = self.checks.iter().map(|c| c.kind.clone()).collect();
        if self.repository.checks == RepositoryChecks::Allow {
            kinds.extend(self.repository.kinds.iter().cloned());
        }
        kinds.sort();
        kinds.dedup();
        kinds
    }

    /// The executor's name: `[executor] name`, else
    /// `reef-executor@<host>`.
    pub fn name(&self) -> String {
        if self.executor.name.is_empty() {
            let host = nix::unistd::gethostname()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| "localhost".into());
            let host: String = host
                .chars()
                .filter(char::is_ascii_graphic)
                .take(100)
                .collect();
            format!("reef-executor@{host}")
        } else {
            self.executor.name.clone()
        }
    }
}

/// The checks a tree's `reef-checks.toml` declares.
#[derive(Clone, Debug)]
pub struct RepoChecks {
    /// The checks, in declaration order.
    pub checks: Vec<Check>,
    /// SHA-256 of the file's bytes.
    pub digest: String,
}

impl RepoChecks {
    /// Parses a repository file's bytes. The message names the line of a
    /// parse error.
    pub fn parse(bytes: &[u8]) -> Result<RepoChecks, String> {
        let digest = reef_evidence::sha256_hex(bytes);
        let text =
            std::str::from_utf8(bytes).map_err(|_| format!("{}: not UTF-8", crate::CHECKS_FILE))?;
        let raw: RawRepoFile =
            toml::from_str(text).map_err(|error| format!("{}: {error}", crate::CHECKS_FILE))?;
        if raw.check.len() > MAX_REPOSITORY_CHECKS {
            return Err(format!(
                "{}: {} checks; a repository declares at most {MAX_REPOSITORY_CHECKS}",
                crate::CHECKS_FILE,
                raw.check.len()
            ));
        }
        let mut checks: Vec<Check> = Vec::new();
        for (index, raw_check) in raw.check.into_iter().enumerate() {
            let check = Check::from_raw(raw_check.into(), Source::Repository)
                .map_err(|m| format!("{}: [[check]] #{}: {m}", crate::CHECKS_FILE, index + 1))?;
            if checks.iter().any(|c| c.name == check.name) {
                return Err(format!(
                    "{}: [[check]] #{}: a second check named {:?}",
                    crate::CHECKS_FILE,
                    index + 1,
                    check.name
                ));
            }
            checks.push(check);
        }
        Ok(RepoChecks { checks, digest })
    }
}

impl Check {
    fn from_raw(raw: RawCheck, source: Source) -> Result<Check, String> {
        let declaration = declaration_digest(&raw);
        if !check_name(&raw.name) {
            return Err(format!(
                "name {:?}: lowercase letters, digits, `_` and `-`, starting with a letter or digit, at most 64",
                raw.name
            ));
        }
        if !reef_evidence::verify::is_kind(&raw.kind) {
            return Err(format!("kind {:?} is not an evidence kind", raw.kind));
        }
        if raw.run.is_none() && raw.run_target.is_none() {
            return Err("neither `run` nor `run_target`: nothing to run".into());
        }
        if raw.run_target.is_some() && raw.targets.is_empty() {
            return Err("`run_target` without `targets`: no target would match".into());
        }
        let mut builder = globset::GlobSetBuilder::new();
        for glob in &raw.targets {
            let glob = globset::GlobBuilder::new(glob)
                .literal_separator(true)
                .build()
                .map_err(|error| format!("targets: {error}"))?;
            builder.add(glob);
        }
        let targets = builder
            .build()
            .map_err(|error| format!("targets: {error}"))?;
        let run = raw
            .run
            .as_deref()
            .map(|argv| argv_templates(argv, false))
            .transpose()?;
        let run_target = raw
            .run_target
            .as_deref()
            .map(|argv| argv_templates(argv, true))
            .transpose()?;
        if let Some(timeout) = raw.timeout
            && timeout < Duration::from_secs(1)
        {
            return Err("timeout: at least 1s".into());
        }
        let mut env = Vec::new();
        for (name, value) in &raw.env {
            if !env_name(name) {
                return Err(format!("env: {name:?} is not a variable name"));
            }
            if name.starts_with("REEF_") {
                return Err(format!(
                    "env: {name} is Reef's; it sets the REEF_CHECK_ variables"
                ));
            }
            if source == Source::Repository && loader_variable(name) {
                return Err(format!(
                    "env: {name} changes how programs are loaded; a repository may not set it"
                ));
            }
            if value.contains('\0') {
                return Err(format!("env: {name}'s value has a NUL"));
            }
            let template = template(value, &[Piece::Cache])?;
            if template.has(&Piece::Cache) && !raw.cache {
                return Err(format!(
                    "env: {name} names {{cache}}, and the check has no cache"
                ));
            }
            env.push((name.clone(), template));
        }
        if raw.artifacts.len() > 16 {
            return Err(format!("artifacts: {}; at most 16", raw.artifacts.len()));
        }
        for artifact in &raw.artifacts {
            if !relative_path(artifact) {
                return Err(format!(
                    "artifacts: {artifact:?} is not a relative path inside the tree"
                ));
            }
        }
        Ok(Check {
            name: raw.name,
            kind: raw.kind,
            targets,
            target_globs: raw.targets,
            run,
            run_target,
            timeout: raw.timeout,
            network: raw.network,
            env,
            artifacts: raw.artifacts,
            error_exit: raw.error_exit,
            cache: raw.cache,
            source,
            declaration,
        })
    }

    /// Whether this check runs `target` (`None` for the whole check), with
    /// the executor's `pattern`.
    pub fn accepts(&self, target: Option<&str>, pattern: &regex::Regex) -> bool {
        match target {
            None => self.run.is_some(),
            Some(target) => {
                self.run_target.is_some()
                    && target_ok(target, pattern)
                    && self.targets.is_match(target)
            }
        }
    }

    /// The argv for `target`, with `{target}` substituted as data.
    pub fn argv(&self, target: Option<&str>) -> Option<Vec<String>> {
        let templates = match target {
            None => self.run.as_ref()?,
            Some(_) => self.run_target.as_ref()?,
        };
        Some(templates.iter().map(|t| t.render(target, None)).collect())
    }
}

/// Whether `target` may reach an argv: the executor's pattern, no `..`
/// component, and neither an option (`-x`) nor an absolute path.
pub fn target_ok(target: &str, pattern: &regex::Regex) -> bool {
    pattern.is_match(target)
        && !target.starts_with('-')
        && !target.starts_with('/')
        && !target.split('/').any(|component| component == "..")
}

/// An argv's templates. `{target}` only in a `run_target`; never in a
/// shell's `-c` script.
fn argv_templates(argv: &[String], targets: bool) -> Result<Vec<Template>, String> {
    let field = if targets { "run_target" } else { "run" };
    if argv.is_empty() {
        return Err(format!("{field} is empty"));
    }
    if argv.len() > 256 {
        return Err(format!("{field}: {} elements; at most 256", argv.len()));
    }
    let allowed: &[Piece] = if targets { &[Piece::Target] } else { &[] };
    let templates = argv
        .iter()
        .map(|element| {
            if element.contains('\0') {
                return Err(format!("{field}: an element has a NUL"));
            }
            template(element, allowed).map_err(|m| format!("{field}: {m}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if templates[0].has(&Piece::Target) {
        return Err(format!("{field}: the program itself cannot be {{target}}"));
    }
    if shell_scripts(argv)
        .into_iter()
        .any(|index| templates[index].has(&Piece::Target))
    {
        return Err(format!(
            "{field}: {{target}} in a shell's -c script builds a command from a request; pass it as an argument after the script instead (\"sh\", \"-c\", \"… \\\"$1\\\"\", \"sh\", \"{{target}}\")"
        ));
    }
    Ok(templates)
}

/// The indexes of the scripts a shell runs with `-c` (or `--command`), if
/// `argv` runs a shell (through `env` too): after each such flag, the first
/// element that is not an option. Conservative: a later `-c` that is the
/// script's own argument counts too.
fn shell_scripts(argv: &[String]) -> Vec<usize> {
    let base = |s: &str| s.rsplit('/').next().unwrap_or(s).to_owned();
    let mut start = 0;
    // `env [-i] [NAME=value]... program`.
    while start < argv.len() && base(&argv[start]) == "env" {
        start += 1;
        while start < argv.len() && (argv[start].starts_with('-') || argv[start].contains('=')) {
            start += 1;
        }
    }
    let Some(program) = argv.get(start).map(|p| base(p)) else {
        return Vec::new();
    };
    if !SHELLS.contains(&program.as_str()) {
        return Vec::new();
    }
    let is_c = |arg: &str| {
        arg == "--command"
            || (arg.len() > 1
                && arg.starts_with('-')
                && !arg.starts_with("--")
                && arg[1..].bytes().all(|b| b.is_ascii_alphabetic())
                && arg[1..].contains('c'))
    };
    let mut scripts = Vec::new();
    for index in start + 1..argv.len() {
        if is_c(&argv[index])
            && let Some(script) = (index + 1..argv.len()).find(|&j| !argv[j].starts_with('-'))
        {
            scripts.push(script);
        }
    }
    scripts
}

/// `text` split into placeholders: `{target}` and `{cache}` where
/// `allowed`, `{{` and `}}` as braces; any other `{…}` is an error.
fn template(text: &str, allowed: &[Piece]) -> Result<Template, String> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut rest = text;
    while let Some(index) = rest.find(['{', '}']) {
        literal.push_str(&rest[..index]);
        let tail = &rest[index..];
        if let Some(after) = tail.strip_prefix("{{") {
            literal.push('{');
            rest = after;
        } else if let Some(after) = tail.strip_prefix("}}") {
            literal.push('}');
            rest = after;
        } else if tail.starts_with('}') {
            return Err(format!("{text:?}: a lone `}}`; write `}}}}` for a brace"));
        } else {
            let Some(end) = tail.find('}') else {
                return Err(format!("{text:?}: an unclosed `{{`"));
            };
            let name = &tail[1..end];
            let piece = match name {
                "target" => Piece::Target,
                "cache" => Piece::Cache,
                _ => {
                    return Err(format!(
                        "{text:?}: unknown placeholder {{{name}}}; write `{{{{` for a brace"
                    ));
                }
            };
            if !allowed.contains(&piece) {
                return Err(format!("{text:?}: {{{name}}} is not allowed here"));
            }
            if !literal.is_empty() {
                pieces.push(Piece::Text(std::mem::take(&mut literal)));
            }
            pieces.push(piece);
            rest = &tail[end + 1..];
        }
    }
    literal.push_str(rest);
    if !literal.is_empty() || pieces.is_empty() {
        pieces.push(Piece::Text(literal));
    }
    Ok(Template(pieces))
}

/// SHA-256 of the check's fields in a fixed JSON form: which declaration
/// produced an object, without recording its argv.
fn declaration_digest(raw: &RawCheck) -> String {
    let canonical = serde_json::json!({
        "name": raw.name,
        "kind": raw.kind,
        "targets": raw.targets,
        "run": raw.run,
        "run_target": raw.run_target,
        "timeout_ms": raw.timeout.map(|t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX)),
        "network": raw.network.as_str(),
        "env": raw.env,
        "artifacts": raw.artifacts,
        "error_exit": raw.error_exit,
        "cache": raw.cache,
    });
    reef_evidence::sha256_hex(canonical.to_string().as_bytes())
}

/// A variable the dynamic loader reads (`LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`
/// and the like).
fn loader_variable(name: &str) -> bool {
    name.starts_with("LD_") || name.starts_with("DYLD_")
}

fn check_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

fn env_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && !bytes[0].is_ascii_digit()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn visible(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && text.bytes().all(|b| b.is_ascii_graphic())
}

/// A relative path with no empty, `.` or `..` component.
pub(crate) fn relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

fn display(path: Option<&Path>) -> String {
    path.map_or_else(
        || "executor configuration".into(),
        |p| p.display().to_string(),
    )
}

/// The user's home directory: `HOME`, else the password database's.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(passwd_home)
}

/// Every directory that is this user's home: `HOME`, and the password
/// database's when it differs. A check sees neither.
pub fn home_dirs() -> Vec<PathBuf> {
    let mut homes: Vec<PathBuf> = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .chain(passwd_home())
        .collect();
    homes.sort();
    homes.dedup();
    homes
}

fn passwd_home() -> Option<PathBuf> {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|user| user.dir)
        .filter(|dir| dir.is_absolute() && dir != Path::new("/"))
}

/// `~` and `~/…` against `home`.
pub fn expand(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home.to_owned(),
        (Some(rest), Some(home)) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(path),
    }
}

/// `<platform cache dir>/reef/executor`.
pub fn default_work_dir() -> Result<PathBuf, Error> {
    use etcetera::BaseStrategy as _;
    let strategy = etcetera::choose_base_strategy().map_err(|error| Error::Config {
        message: format!("no home directory for the default [work] dir: {error}"),
    })?;
    #[cfg(target_os = "macos")]
    let cache = strategy.home_dir().join("Library/Caches");
    #[cfg(not(target_os = "macos"))]
    let cache = strategy.cache_dir();
    Ok(cache.join("reef").join("executor"))
}

/// `"4GiB"`, `"16MiB"`, `"512KiB"`, `"100B"`, or a bare integer of bytes.
fn size<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Bytes(u64),
        Text(String),
    }
    match Raw::deserialize(deserializer)? {
        Raw::Bytes(bytes) => Ok(bytes),
        Raw::Text(text) => parse_size(&text).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "{text:?} is not a size such as \"4GiB\", \"16MiB\" or \"512KiB\""
            ))
        }),
    }
}

/// A size in bytes from `"4GiB"` and the like.
pub fn parse_size(text: &str) -> Option<u64> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, String> {
        Config::parse(text, None).map_err(|e| e.to_string())
    }

    #[test]
    fn every_key_parses() {
        let config = parse(
            r#"
[executor]
name = "laptop"
labels = ["gpu=none"]
repos = ["r1"]
identity = "ci-main"
lease = "90s"

[repository]
checks = "deny"
kinds = ["test"]

[declarations]
target_pattern = '^[a-z/]+$'

[materialize]
verify = "listing"
cache_size = "1GiB"

[work]
dir = "/tmp/reef-work"

[sandbox]
kind = "none"
bwrap = "/usr/bin/bwrap"
read = ["~/.rustup"]
pass_env = ["CARGO_HOME"]
env = { RUSTUP_HOME = "~/.rustup", LANG = "C" }
max_repository_network = "loopback"

[run]
max_log = "1MiB"
max_artifact = 1024
kill_grace = "2s"

[[check]]
name = "clippy"
kind = "lint"
run = ["cargo", "clippy"]
cache = true
env = { CARGO_TARGET_DIR = "{cache}/target" }
timeout = "20m"
network = "full"
artifacts = ["out/report.xml"]
error_exit = [75]

[[check]]
name = "crate"
kind = "test"
targets = ["crates/*"]
run_target = ["cargo", "test", "--manifest-path", "{target}/Cargo.toml"]
"#,
        )
        .unwrap();
        assert_eq!(config.executor.lease, Duration::from_secs(90));
        assert_eq!(config.repository.checks, RepositoryChecks::Deny);
        assert_eq!(config.materialize.cache_size, 1 << 30);
        assert_eq!(config.run.max_artifact, 1024);
        assert_eq!(config.checks.len(), 2);
        assert_eq!(config.kinds(), vec!["lint", "test"]);
        let crate_check = &config.checks[1];
        assert!(crate_check.accepts(Some("crates/reef"), &config.target_pattern));
        assert_eq!(
            crate_check.argv(Some("crates/reef")).unwrap(),
            vec!["cargo", "test", "--manifest-path", "crates/reef/Cargo.toml"]
        );
        let clippy = &config.checks[0];
        assert_eq!(
            clippy.env[0].1.render(None, Some(Path::new("/c"))),
            "/c/target"
        );
    }

    #[test]
    fn defaults_claim_repository_kinds() {
        let config = Config::defaults().unwrap();
        assert_eq!(config.kinds(), vec!["build", "lint", "test"]);
        assert_eq!(config.executor.lease, Duration::from_mins(1));
        assert!(config.name().starts_with("reef-executor@"));
    }

    #[test]
    fn refusals_name_their_line_or_field() {
        let cases: &[(&str, &str)] = &[
            ("[executor]\nnope = 1\n", "line 2"),
            ("[sandbox]\npass_env = [\"REEF_X\"]\n", "REEF_X is Reef's"),
            (
                "[sandbox]\nenv = { REEF_SOCKET = \"x\" }\n",
                "not a variable name a check may get",
            ),
            ("[executor]\nlease = \"5s\"\n", "15s to 10m"),
            (
                "[[check]]\nname = \"a\"\nkind = \"test\"\nrun = [\"x\"]\n[[check]]\nname = \"a\"\nkind = \"test\"\nrun = [\"y\"]\n",
                "a second check named",
            ),
            (
                "[[check]]\nname = \"a\"\nkind = \"test\"\nrun = [\"x\", \"{foo}\"]\n",
                "unknown placeholder {foo}",
            ),
            (
                "[[check]]\nname = \"a\"\nkind = \"test\"\nrun = [\"x\", \"{target}\"]\n",
                "{target} is not allowed here",
            ),
            (
                "[[check]]\nname = \"Bad\"\nkind = \"test\"\nrun = [\"x\"]\n",
                "lowercase letters",
            ),
            (
                "[[check]]\nname = \"a\"\nkind = \"test\"\nrun = [\"x\"]\nenv = { X = \"{cache}\" }\n",
                "has no cache",
            ),
            (
                "[[check]]\nname = \"a\"\nkind = \"test\"\nrun = [\"x\"]\nartifacts = [\"../x\"]\n",
                "not a relative path",
            ),
        ];
        for (text, wanted) in cases {
            let error = parse(text).unwrap_err();
            assert!(error.contains(wanted), "{text:?}: {error}");
        }
    }

    #[test]
    fn repository_files_cannot_ask_for_a_cache_or_too_many_checks() {
        let error = RepoChecks::parse(
            b"[[check]]\nname = \"a\"\nkind = \"test\"\nrun = [\"x\"]\ncache = true\n",
        )
        .unwrap_err();
        assert!(error.contains("unknown field `cache`"), "{error}");
        assert!(error.contains("line 5"), "{error}");
        let mut many = String::new();
        for i in 0..65 {
            use std::fmt::Write as _;
            let _ = write!(
                many,
                "[[check]]\nname = \"c{i}\"\nkind = \"test\"\nrun = [\"x\"]\n"
            );
        }
        let error = RepoChecks::parse(many.as_bytes()).unwrap_err();
        assert!(error.contains("65 checks"), "{error}");
        let error = RepoChecks::parse(
            b"[[check]]\nname = \"a\"\nkind = \"test\"\nrun = [\"x\"]\nenv = { LD_PRELOAD = \"./evil.so\" }\n",
        )
        .unwrap_err();
        assert!(error.contains("how programs are loaded"), "{error}");
        let ok = RepoChecks::parse(b"[[check]]\nname = \"a\"\nkind = \"test\"\nrun = [\"x\"]\n")
            .unwrap();
        assert_eq!(ok.checks[0].source, Source::Repository);
        assert_eq!(ok.digest.len(), 64);
    }

    #[test]
    fn targets_are_data() {
        let pattern = regex::Regex::new(DEFAULT_TARGET_PATTERN).unwrap();
        let check = Check::from_raw(
            RawCheck {
                name: "c".into(),
                kind: "test".into(),
                targets: vec!["crates/*".into(), "*".into()],
                run: None,
                run_target: Some(vec![
                    "cargo".into(),
                    "--package={target}".into(),
                    "{{x}}".into(),
                ]),
                timeout: None,
                network: Network::None,
                env: BTreeMap::new(),
                artifacts: Vec::new(),
                error_exit: Vec::new(),
                cache: false,
            },
            Source::Executor,
        )
        .unwrap();
        assert!(check.accepts(Some("crates/reef-lib"), &pattern));
        assert!(!check.accepts(Some("crates/reef-lib/tests"), &pattern));
        assert!(!check.accepts(None, &pattern));
        for bad in [
            "crates/a b",
            "crates/$(id)",
            "crates/;rm",
            "crates/x\ny",
            "crates/..",
            "../etc",
            "-x",
            "--config=/etc/passwd",
            "/etc",
        ] {
            assert!(!check.accepts(Some(bad), &pattern), "{bad:?}");
        }
        assert!(!check.accepts(Some(&format!("crates/{}", "a".repeat(510))), &pattern));
        assert_eq!(
            check.argv(Some("crates/x")).unwrap(),
            vec!["cargo", "--package=crates/x", "{x}"]
        );
    }

    #[test]
    fn shell_scripts_never_hold_a_target() {
        let refused = [
            vec!["sh", "-c", "test {target}"],
            vec!["/bin/bash", "-c", "{target}"],
            vec!["bash", "-ec", "echo {target}"],
            vec!["env", "-i", "A=1", "sh", "-c", "x {target}"],
            vec!["/usr/bin/env", "zsh", "-c", "{target}"],
            vec!["bash", "-o", "pipefail", "-c", "{target}"],
            vec!["sh", "-c", "--", "{target}"],
        ];
        for argv in refused {
            let argv: Vec<String> = argv.into_iter().map(str::to_owned).collect();
            let error = argv_templates(&argv, true).unwrap_err();
            assert!(error.contains("shell's -c script"), "{argv:?}: {error}");
        }
        let allowed: Vec<String> = ["sh", "-c", "cargo test -p \"$1\"", "sh", "{target}"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        argv_templates(&allowed, true).unwrap();
        let program: Vec<String> = vec!["{target}".into()];
        assert!(argv_templates(&program, true).is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("4GiB"), Some(4 << 30));
        assert_eq!(parse_size("16MiB"), Some(16 << 20));
        assert_eq!(parse_size("100"), Some(100));
        assert_eq!(parse_size("1 KiB"), Some(1024));
        assert_eq!(parse_size("1GB"), None);
    }
}
