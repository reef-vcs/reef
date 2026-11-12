//! The repositories a fleet run builds: the pinned public ones in
//! `fleet/repos.toml`, Reef's own checkout (reported, not gated), and a
//! synthetic one generated here, small enough for `just ci`.
//!
//! Each repository has five tickets, `fleet/patches/<name>/<n>.patch`, and
//! a review round per ticket, `<n>b.patch`, applied on top. The synthetic
//! repository writes its own patches.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// `fleet/repos.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Every repository, in the order a run takes them.
    #[serde(rename = "repo")]
    pub repos: Vec<RepoSpec>,
}

/// The manifest shipped with the harness.
pub const MANIFEST: &str = include_str!("../../fleet/repos.toml");

impl Manifest {
    /// Parses a manifest.
    pub fn parse(text: &str) -> Result<Self, String> {
        let manifest: Manifest = toml::from_str(text).map_err(|e| format!("repos.toml: {e}"))?;
        let mut names = std::collections::HashSet::new();
        for repo in &manifest.repos {
            if !names.insert(repo.name.as_str()) {
                return Err(format!("repos.toml: `{}` is listed twice", repo.name));
            }
            if repo.name == SYNTHETIC || repo.name == "all" {
                return Err(format!("repos.toml: `{}` is a reserved name", repo.name));
            }
            if repo.url != SELF && repo.rev.len() != 40 {
                return Err(format!(
                    "repos.toml: `{}` must pin a full commit id, not `{}`",
                    repo.name, repo.rev
                ));
            }
        }
        Ok(manifest)
    }

    /// The shipped manifest.
    pub fn shipped() -> Result<Self, String> {
        Self::parse(MANIFEST)
    }
}

/// The language a repository's build is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stack {
    /// Cargo.
    Rust,
    /// The go command.
    Go,
    /// A Node package manager.
    Typescript,
    /// uv.
    Python,
}

/// One repository.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoSpec {
    /// Short name: `--repo <name>`, and the patches' directory.
    pub name: String,
    /// Its stack.
    pub stack: Stack,
    /// Where to clone from; `self` is Reef's own checkout (`--reef-checkout`).
    pub url: String,
    /// The full commit id to check out; ignored for `self` (the checkout's
    /// `HEAD`).
    #[serde(default)]
    pub rev: String,
    /// The repository's license, recorded in the report.
    #[serde(default)]
    pub license: String,
    /// Whether its row is judged against the gates (Reef's own is reported
    /// only).
    #[serde(default = "yes")]
    pub gated: bool,
    /// Programs that must be on `PATH`.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Shell lines run once in a fresh clone, untimed: downloads and
    /// caches. `$FLEET_PATCHES` is the repository's patches directory.
    #[serde(default)]
    pub setup: Vec<String>,
    /// Shell lines run before each scenario, untimed: empties shared build
    /// caches (such as `GOCACHE`) that are build output, not downloads.
    #[serde(default)]
    pub clean: Vec<String>,
    /// The timed build and tests, a shell line run in the workspace.
    pub build: String,
    /// Directories (relative) holding build output, measured apart and
    /// cloned by the spike's seeds.
    #[serde(default)]
    pub build_dirs: Vec<String>,
    /// Variables the repository's build always runs with.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// For `--seed clone`: a shell line, run in a built workspace, that
    /// leaves only dependencies' build output (the shape of a clone store).
    #[serde(default)]
    pub seed_prepare: Option<String>,
    /// Shared tool caches to measure at the end (`~` expanded), such as
    /// `GOCACHE`.
    #[serde(default)]
    pub caches: Vec<String>,
    /// For `--stores on`: a `reef.toml` committed to the clone before the
    /// run, whose stores are built first.
    #[serde(default)]
    pub reef_toml: Option<String>,
}

fn yes() -> bool {
    true
}

/// `spec` with `{caches}` in its `env` and `caches` replaced by `dir`: the
/// run's own directory for tool caches the harness must not share with the
/// user's (such as `GOCACHE`, which `clean` empties).
pub fn with_caches(mut spec: RepoSpec, dir: &Path) -> RepoSpec {
    let dir = dir.to_string_lossy();
    for value in spec.env.values_mut() {
        *value = value.replace("{caches}", &dir);
    }
    for cache in &mut spec.caches {
        *cache = cache.replace("{caches}", &dir);
    }
    spec
}

/// The branch a clone's workspaces start on.
pub const REF: &str = "main";

/// The generated repository's name.
pub const SYNTHETIC: &str = "synthetic";
/// `url` for Reef's own checkout.
pub const SELF: &str = "self";

/// The synthetic repository's spec: five tiny crates and a probe crate (the
/// spike's E1 and E2 fixtures), and a tiny Node package when `node` is on
/// `PATH`.
pub fn synthetic() -> RepoSpec {
    RepoSpec {
        name: SYNTHETIC.to_owned(),
        stack: Stack::Rust,
        url: SYNTHETIC.to_owned(),
        rev: String::new(),
        license: "generated".to_owned(),
        gated: true,
        tools: vec!["cargo".to_owned(), "git".to_owned()],
        setup: vec![],
        clean: vec![],
        build: "cargo test --workspace --offline --locked && \
                if command -v node >/dev/null 2>&1; then cd js && node --test --test-reporter=tap; fi"
            .to_owned(),
        build_dirs: vec!["target".to_owned()],
        env: BTreeMap::new(),
        seed_prepare: Some(
            "cargo clean --offline -p c1 -p c2 -p c3 -p c4 -p c5 -p probe".to_owned(),
        ),
        caches: vec![],
        reef_toml: None,
    }
}

/// A repository ready to run: its clone and its patches.
#[derive(Clone, Debug)]
pub struct Prepared {
    /// The spec.
    pub spec: RepoSpec,
    /// The clone (a git repository Reef registers).
    pub dir: PathBuf,
    /// The pinned commit.
    pub rev: String,
    /// The commit workspaces start on: the pinned one, or a commit on it
    /// that setup made (tokio's `Cargo.lock`).
    pub head: String,
    /// Where `<n>.patch` and `<n>b.patch` are.
    pub patches: PathBuf,
}

impl Prepared {
    /// Ticket `n`'s patch for `round` (1, or 2 for `<n>b.patch`).
    pub fn patch(&self, ticket: u32, round: u32) -> PathBuf {
        let suffix = if round == 1 { "" } else { "b" };
        self.patches.join(format!("{ticket}{suffix}.patch"))
    }
}

/// Clones (or generates) `spec` under `cache`, checks it out, checks its
/// patches and runs its setup. An existing clone at the right commit is
/// reused.
pub fn prepare(
    spec: &RepoSpec,
    cache: &Path,
    patches_root: &Path,
    reef_checkout: &Path,
    tickets: u32,
    env: &[(String, std::ffi::OsString)],
) -> Result<Prepared, String> {
    for tool in &spec.tools {
        if which(tool).is_none() {
            return Err(format!("`{}` needs `{tool}` on PATH", spec.name));
        }
    }
    let dir = cache.join(&spec.name);
    let marker = dir.join(".git/reef-fleet-setup");
    let (rev, patches) = if spec.url == SYNTHETIC {
        let patches = cache.join(format!("{}-patches", spec.name));
        let rev = generate_synthetic(&dir, &patches, env)?;
        (rev, patches)
    } else {
        let (url, rev) = if spec.url == SELF {
            let rev = git(&["rev-parse", "HEAD"], reef_checkout, env)?;
            (reef_checkout.to_string_lossy().into_owned(), rev)
        } else {
            (spec.url.clone(), spec.rev.clone())
        };
        let patches = patches_root.join(&spec.name);
        // A clone set up at this commit before is reused, reset to the
        // commit its setup left (a `--stores on` run commits on top).
        // The marker holds the pinned commit, then the set-up head.
        let recorded = fs::read_to_string(&marker).unwrap_or_default();
        let mut lines = recorded.lines();
        let reuse = (lines.next() == Some(rev.as_str()))
            .then(|| lines.next().map(str::to_owned))
            .flatten();
        if let Some(head) = &reuse {
            git(
                &["checkout", "--quiet", "--force", "-B", REF, head],
                &dir,
                env,
            )?;
        } else {
            if dir.exists() {
                fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            fs::create_dir_all(cache).map_err(|e| format!("{}: {e}", cache.display()))?;
            git(
                &[
                    "clone",
                    "--quiet",
                    "--no-checkout",
                    &url,
                    &dir.to_string_lossy(),
                ],
                cache,
                env,
            )?;
            // `main` at the pinned commit, and no other local branch, so a
            // workspace's default start is the pinned commit too.
            git(&["checkout", "--quiet", "-B", REF, &rev], &dir, env)?;
            let others = git(
                &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
                &dir,
                env,
            )?;
            for branch in others.lines().filter(|b| *b != REF) {
                git(&["branch", "--quiet", "-D", branch], &dir, env)?;
            }
            let mut extra = spec.env.clone();
            extra.insert(
                "FLEET_PATCHES".to_owned(),
                patches.to_string_lossy().into_owned(),
            );
            for line in &spec.setup {
                shell(line, &dir, env, &extra).map_err(|e| format!("{} setup: {e}", spec.name))?;
            }
            let head = git(&["rev-parse", "HEAD"], &dir, env)?;
            fs::write(&marker, format!("{rev}\n{head}\n"))
                .map_err(|e| format!("{}: {e}", marker.display()))?;
        }
        (rev, patches)
    };
    for ticket in 1..=tickets {
        for round in [1, 2] {
            let suffix = if round == 1 { "" } else { "b" };
            let patch = patches.join(format!("{ticket}{suffix}.patch"));
            if !patch.is_file() {
                return Err(format!("`{}` has no {}", spec.name, patch.display()));
            }
        }
    }
    let head = git(&["rev-parse", "HEAD"], &dir, env)?;
    Ok(Prepared {
        spec: spec.clone(),
        dir,
        rev,
        head,
        patches,
    })
}

/// Checks that every patch applies, in order, to a fresh checkout of the
/// prepared repository: ticket `n`, then its review round.
pub fn check_patches(
    prepared: &Prepared,
    scratch: &Path,
    tickets: u32,
    env: &[(String, std::ffi::OsString)],
) -> Result<(), String> {
    for ticket in 1..=tickets {
        let dir = scratch.join(format!("check-{ticket}"));
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        git(
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                &dir.to_string_lossy(),
                &prepared.head,
            ],
            &prepared.dir,
            env,
        )?;
        let result = (|| -> Result<(), String> {
            for round in [1, 2] {
                let patch = prepared.patch(ticket, round);
                git(&["apply", "--check", &patch.to_string_lossy()], &dir, env)
                    .map_err(|e| format!("{}: {e}", patch.display()))?;
                git(&["apply", &patch.to_string_lossy()], &dir, env)
                    .map_err(|e| format!("{}: {e}", patch.display()))?;
            }
            Ok(())
        })();
        let _ = git(
            &["worktree", "remove", "--force", &dir.to_string_lossy()],
            &prepared.dir,
            env,
        );
        result?;
    }
    Ok(())
}

/// Runs git in `cwd`, returning trimmed stdout.
pub fn git(
    args: &[&str],
    cwd: &Path,
    env: &[(String, std::ffi::OsString)],
) -> Result<String, String> {
    crate::agent::process::output(
        Command::new("git")
            .args([
                "-c",
                "user.name=Fleet",
                "-c",
                "user.email=fleet@example.com",
            ])
            .args([
                "-c",
                "advice.detachedHead=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(env.iter().cloned()),
    )
}

/// Runs a shell line in `cwd`.
pub fn shell(
    line: &str,
    cwd: &Path,
    env: &[(String, std::ffi::OsString)],
    extra: &BTreeMap<String, String>,
) -> Result<String, String> {
    crate::agent::process::output(
        Command::new("sh")
            .args(["-c", line])
            .current_dir(cwd)
            .env_clear()
            .envs(env.iter().cloned())
            .envs(extra),
    )
}

/// `program`'s path on `PATH`.
pub fn which(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| path.is_file())
}

/// One synthetic crate's `lib.rs` at a stage: 0 the base, 1 after its
/// ticket's patch, 2 after the review round.
fn crate_source(n: u32, stage: u32) -> String {
    let value = if stage == 0 { n } else { n * 10 };
    let test = if stage == 0 {
        "value_is_base"
    } else {
        "value_is_patched"
    };
    let doubled = if stage == 2 {
        "\n/// Twice the value.\npub fn doubled() -> u32 {\n    value() * 2\n}\n"
    } else {
        ""
    };
    let doubled_test = if stage == 2 {
        format!(
            "\n    #[test]\n    fn doubled_is_patched() {{\n        assert_eq!(super::doubled(), {});\n    }}\n",
            value * 2
        )
    } else {
        String::new()
    };
    format!(
        "//! Synthetic crate {n} of the fleet benchmark.\n\n/// The crate's value.\npub fn value() -> u32 {{\n    {value}\n}}\n{doubled}\n#[cfg(test)]\nmod tests {{\n    #[test]\n    fn {test}() {{\n        assert_eq!(super::value(), {value});\n    }}\n{doubled_test}}}\n"
    )
}

/// The probe crate: fails when its test binary was built in another
/// workspace (the spike's E1: a reused artifact carries the other
/// workspace's `CARGO_MANIFEST_DIR`).
const PROBE: &str = r#"//! The fleet benchmark's probe: was this test binary built here?

#[cfg(test)]
mod tests {
    #[test]
    fn manifest_dir_is_this_workspace() {
        let built = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
        let here = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        assert_eq!(built, here, "this test binary was built in another directory");
    }
}
"#;

/// Writes the synthetic repository at `dir` (a git repository with one
/// commit) and its patches in `patches`; returns the commit. Rewritten
/// from scratch each time, so the patches always match.
fn generate_synthetic(
    dir: &Path,
    patches: &Path,
    env: &[(String, std::ffi::OsString)],
) -> Result<String, String> {
    let write = |path: &Path, text: &str| {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
    };
    for path in [dir, patches] {
        if path.exists() {
            fs::remove_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let members: Vec<String> = (1..=5)
        .map(|n| format!("\"c{n}\""))
        .chain(["\"probe\"".to_owned()])
        .collect();
    write(
        &dir.join("Cargo.toml"),
        &format!(
            "[workspace]\nmembers = [{}]\nresolver = \"2\"\n",
            members.join(", ")
        ),
    )?;
    let manifest = |name: &str| {
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\npublish = false\n"
        )
    };
    for n in 1..=5 {
        let name = format!("c{n}");
        write(&dir.join(&name).join("Cargo.toml"), &manifest(&name))?;
        write(&dir.join(&name).join("src/lib.rs"), &crate_source(n, 0))?;
    }
    write(&dir.join("probe/Cargo.toml"), &manifest("probe"))?;
    write(&dir.join("probe/src/lib.rs"), PROBE)?;
    write(
        &dir.join("js/package.json"),
        "{\n  \"name\": \"synthetic\",\n  \"private\": true,\n  \"scripts\": { \"test\": \"node --test\" }\n}\n",
    )?;
    write(
        &dir.join("js/sum.test.mjs"),
        "import test from 'node:test';\nimport assert from 'node:assert';\n\ntest('adds', () => {\n  assert.strictEqual(1 + 1, 2);\n});\n",
    )?;
    write(&dir.join(".gitignore"), "target/\nnode_modules/\n")?;
    write(
        &dir.join("README.md"),
        "# Synthetic\n\nGenerated by reef-fleet.\n",
    )?;
    shell(
        "cargo generate-lockfile --offline",
        dir,
        env,
        &BTreeMap::new(),
    )?;
    git(&["init", "--quiet"], dir, env)?;
    git(&["add", "-A"], dir, env)?;
    git(
        &["commit", "--quiet", "-m", "Synthetic repository"],
        dir,
        env,
    )?;
    for n in 1..=5 {
        let lib = dir.join(format!("c{n}/src/lib.rs"));
        write(&lib, &crate_source(n, 1))?;
        let first = git(&["diff"], dir, env)?;
        write(&patches.join(format!("{n}.patch")), &format!("{first}\n"))?;
        git(&["add", "-A"], dir, env)?;
        write(&lib, &crate_source(n, 2))?;
        let second = git(&["diff"], dir, env)?;
        write(&patches.join(format!("{n}b.patch")), &format!("{second}\n"))?;
        git(&["reset", "--quiet", "--hard"], dir, env)?;
    }
    git(&["rev-parse", "HEAD"], dir, env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_manifest_parses() {
        let manifest = Manifest::shipped().unwrap();
        let stacks: Vec<Stack> = manifest.repos.iter().map(|r| r.stack).collect();
        for stack in [Stack::Rust, Stack::Go, Stack::Typescript, Stack::Python] {
            assert!(stacks.contains(&stack), "no {stack:?} repository");
        }
        assert!(
            manifest.repos.iter().any(|r| r.url == SELF && !r.gated),
            "Reef's own row is reported, not gated"
        );
    }

    #[test]
    fn caches_are_the_runs_own() {
        let cli = Manifest::shipped()
            .unwrap()
            .repos
            .into_iter()
            .find(|r| r.name == "cli")
            .unwrap();
        let cli = with_caches(cli, Path::new("/w/caches"));
        assert_eq!(cli.env["GOCACHE"], "/w/caches/go-build");
        assert_eq!(cli.caches, vec!["/w/caches/go-build".to_owned()]);
    }

    #[test]
    fn a_manifest_must_pin_commits_and_unique_names() {
        let repo = |name: &str, rev: &str| {
            format!(
                "[[repo]]\nname = \"{name}\"\nstack = \"go\"\nurl = \"https://example.com/x\"\nrev = \"{rev}\"\nbuild = \"go test ./...\"\n"
            )
        };
        let full = "0123456789012345678901234567890123456789";
        assert!(Manifest::parse(&repo("x", full)).is_ok());
        assert!(
            Manifest::parse(&repo("x", "v1.0"))
                .unwrap_err()
                .contains("full commit")
        );
        let twice = format!("{}{}", repo("x", full), repo("x", full));
        assert!(Manifest::parse(&twice).unwrap_err().contains("twice"));
        assert!(
            Manifest::parse(&repo("synthetic", full))
                .unwrap_err()
                .contains("reserved")
        );
        assert!(Manifest::parse("[[repo]]\nname = \"x\"\nbogus = 1\n").is_err());
    }

    #[test]
    fn the_synthetic_patches_apply_in_order() {
        if which("cargo").is_none() || which("git").is_none() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let env: Vec<(String, std::ffi::OsString)> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v)))
            .filter(|(k, _)| !k.starts_with("GIT_"))
            .collect();
        let prepared =
            prepare(&synthetic(), root.path(), root.path(), root.path(), 5, &env).unwrap();
        check_patches(&prepared, root.path(), 5, &env).unwrap();
        let patch = fs::read_to_string(prepared.patch(3, 2)).unwrap();
        assert!(patch.contains("doubled_is_patched"), "{patch}");
    }
}
