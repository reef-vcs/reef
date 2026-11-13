//! The variables that put a command under the build limit (#271, ADR
//! 0074): the jobserver for cargo, and job counts for tools without one,
//! each left out where a setting already made would be overridden.
//!
//! The environment overrides what files say: an inherited jobserver
//! silently lifts a `build.jobs` cap, `RUST_TEST_THREADS` overrides a
//! repository's `[env]` request for serial tests, and `GOFLAGS` replaces
//! the user's `go env -w` flags (such as `-trimpath`, which changes the
//! build). So the workspace's and the user's files are read first, and a
//! variable that would override one of them is not set.

use std::path::{Path, PathBuf};

use reef_api::v1::{BuildVariable, BuildVariableMerge, JobserverSkip};
use reef_lib::spec::BuildSpec;

use super::cargo_config::Keys;
use super::make::{self, Make};

/// Files larger than this are treated as absent.
const MAX_FILE: u64 = 1 << 20;

/// What a command's environment should hold, and why the jobserver is
/// missing if it is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Computed {
    /// In the order to apply them.
    pub variables: Vec<BuildVariable>,
    /// `Unspecified` when the jobserver is offered.
    pub skip: JobserverSkip,
    /// One sentence for a person when the jobserver is missing.
    pub note: String,
}

/// What [`compute`] works from.
pub(crate) struct Inputs<'a> {
    /// The machine's job count.
    pub jobs: u32,
    /// max(2, jobs / builders).
    pub share: u32,
    /// The jobserver's path, or why there is none machine-wide.
    pub jobserver: Result<&'a Path, (JobserverSkip, String)>,
    /// `[build] make_jobserver`, checked at start.
    pub make_jobserver: bool,
    /// The repository's `[build]`.
    pub spec: &'a BuildSpec,
    /// The workspace's directory; `None` for the machine's defaults.
    pub workspace: Option<&'a Path>,
    /// The user's cargo and go settings.
    pub user: &'a UserFiles,
    /// The keys the cargo file carries, when it reaches the workspace.
    pub cargo_file: Option<Keys>,
    /// The machine's `make`, asked only for a workspace with a hazard.
    pub make: &'a dyn Fn() -> Make,
}

/// The variables for one command.
pub(crate) fn compute(inputs: &Inputs<'_>) -> Computed {
    let workspace = inputs
        .workspace
        .map(WorkspaceFiles::read)
        .unwrap_or_default();
    let user = inputs.user;
    let file_keys = inputs.cargo_file.unwrap_or_default();
    let share = inputs.share.to_string();
    let mut computed = Computed::default();
    let jobserver = match &inputs.jobserver {
        Err((skip, note)) => Err((*skip, note.clone())),
        Ok(_) if !inputs.spec.jobserver => Err((
            JobserverSkip::Repository,
            "the repository's reef.toml says [build] jobserver = false".to_owned(),
        )),
        Ok(_) if workspace.cargo_jobs.is_some() || user.cargo_jobs.is_some() => {
            let path = workspace.cargo_jobs.as_ref().or(user.cargo_jobs.as_ref());
            Err((
                JobserverSkip::CargoJobsSet,
                format!(
                    "build.jobs is set in {}, a cap a jobserver would lift",
                    path.map_or_else(String::new, |path| path.display().to_string())
                ),
            ))
        }
        Ok(path) => match workspace.hazard {
            Some(package) if !inputs.make_jobserver => {
                let make = (inputs.make)();
                let found = make.for_package(package);
                if found.modern {
                    Ok(*path)
                } else {
                    Err((
                        JobserverSkip::MakeTooOld,
                        format!(
                            "{package} hands the jobserver to make, and the one it would run here is {}, which cannot read it: \
                             the counts still apply; put GNU make 4.4 or later first on PATH as {} to get the jobserver",
                            found.text,
                            if package == "cmake" {
                                "gmake or make"
                            } else {
                                "make"
                            }
                        ),
                    ))
                }
            }
            _ => Ok(*path),
        },
    };
    match jobserver {
        Ok(path) => {
            let flags = format!("-j --jobserver-auth=fifo:{}", path.display());
            computed
                .variables
                .push(if_unset("CARGO_MAKEFLAGS", flags.clone()));
            if inputs.make_jobserver {
                computed.variables.push(if_unset("MAKEFLAGS", flags));
            }
        }
        Err((skip, note)) => {
            computed.skip = skip;
            computed.note = note;
        }
    }
    // Under the cargo file the counts come from it: cargo reads it at every
    // invocation and it follows the builders, where a variable is fixed at
    // the command's start.
    if workspace.cargo_jobs.is_none() && user.cargo_jobs.is_none() && !file_keys.jobs {
        computed
            .variables
            .push(if_unset("CARGO_BUILD_JOBS", share.clone()));
    }
    if !workspace.rust_test_threads && !user.rust_test_threads && !file_keys.rust_test_threads {
        computed
            .variables
            .push(if_unset("RUST_TEST_THREADS", share.clone()));
    }
    if !workspace.nextest_threads {
        computed
            .variables
            .push(if_unset("NEXTEST_TEST_THREADS", share.clone()));
    }
    // Always sent: a `GOFLAGS` in the command's environment (which
    // overrides the file) still gets `-p` appended. The value, for an
    // environment without one, is empty where the file already has a `-p`
    // or cannot be read: then nothing is set.
    let goflags = match &user.goflags {
        GoFlags::Unreadable => String::new(),
        GoFlags::Flags(flags) if has_p_flag(flags) => String::new(),
        GoFlags::Flags(flags) if flags.trim().is_empty() => format!("-p={share}"),
        GoFlags::Flags(flags) => format!("{} -p={share}", flags.trim()),
    };
    computed.variables.push(BuildVariable {
        name: "GOFLAGS".to_owned(),
        value: goflags,
        merge: BuildVariableMerge::Goflags.into(),
    });
    for (name, value) in &inputs.spec.jobs_env {
        let value = match value.as_str() {
            "{jobs}" => inputs.jobs.to_string(),
            _ => share.clone(),
        };
        computed.variables.push(if_unset(name, value));
    }
    computed
}

fn if_unset(name: &str, value: String) -> BuildVariable {
    BuildVariable {
        name: name.to_owned(),
        value,
        merge: BuildVariableMerge::IfUnset.into(),
    }
}

/// Whether `flags` (a `GOFLAGS` value) already has a `-p` flag.
pub(crate) fn has_p_flag(flags: &str) -> bool {
    flags.split_whitespace().any(|flag| {
        let flag = flag.trim_start_matches('-');
        flag == "p" || flag.starts_with("p=")
    })
}

/// What the workspace's own files say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct WorkspaceFiles {
    /// The file that sets `build.jobs`.
    cargo_jobs: Option<PathBuf>,
    /// `.cargo/config.toml`'s `[env]` sets `RUST_TEST_THREADS`.
    rust_test_threads: bool,
    /// `.config/nextest.toml` sets `test-threads` in some profile.
    nextest_threads: bool,
    /// A package in the root `Cargo.lock` that hands the jobserver to make.
    hazard: Option<&'static str>,
}

impl WorkspaceFiles {
    fn read(dir: &Path) -> WorkspaceFiles {
        let mut files = WorkspaceFiles::default();
        for name in ["config.toml", "config"] {
            let path = dir.join(".cargo").join(name);
            let Some(config) = read_toml(&path) else {
                continue;
            };
            let cargo = CargoConfig::of(&config);
            if cargo.jobs && files.cargo_jobs.is_none() {
                files.cargo_jobs = Some(path);
            }
            files.rust_test_threads |= cargo.rust_test_threads;
        }
        files.nextest_threads =
            read_toml(&dir.join(".config").join("nextest.toml")).is_some_and(|config| {
                config
                    .get("profile")
                    .and_then(toml::Value::as_table)
                    .is_some_and(|profiles| {
                        profiles.values().any(|profile| {
                            profile
                                .as_table()
                                .is_some_and(|profile| profile.contains_key("test-threads"))
                        })
                    })
            });
        files.hazard =
            read_small(&dir.join("Cargo.lock")).and_then(|text| make::hazard_in_lockfile(&text));
        files
    }
}

/// What one cargo configuration file sets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CargoConfig {
    /// `build.jobs`.
    pub jobs: bool,
    /// `[env] RUST_TEST_THREADS`.
    pub rust_test_threads: bool,
}

impl CargoConfig {
    fn of(config: &toml::Table) -> CargoConfig {
        CargoConfig {
            jobs: config
                .get("build")
                .and_then(toml::Value::as_table)
                .is_some_and(|build| build.contains_key("jobs")),
            rust_test_threads: config
                .get("env")
                .and_then(toml::Value::as_table)
                .is_some_and(|env| env.contains_key("RUST_TEST_THREADS")),
        }
    }
}

/// The user's go settings: `GOFLAGS` from the go environment file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GoFlags {
    /// The flags (empty when there is no file or no `GOFLAGS` in it).
    Flags(String),
    /// The file is there but cannot be read: `GOFLAGS` is not set.
    Unreadable,
}

/// What the user's own configuration says, read per request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UserFiles {
    /// The `$CARGO_HOME` file that sets `build.jobs`.
    pub cargo_jobs: Option<PathBuf>,
    /// `$CARGO_HOME`'s config sets `[env] RUST_TEST_THREADS`.
    pub rust_test_threads: bool,
    /// The go environment file's `GOFLAGS`.
    pub goflags: GoFlags,
}

impl Default for UserFiles {
    fn default() -> UserFiles {
        UserFiles {
            cargo_jobs: None,
            rust_test_threads: false,
            goflags: GoFlags::Flags(String::new()),
        }
    }
}

impl UserFiles {
    /// The daemon user's files: `$CARGO_HOME` (default `~/.cargo`) and
    /// the go environment file (`$GOENV`, default
    /// `<user config dir>/go/env`; none when `GOENV=off`).
    pub(crate) fn read() -> UserFiles {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".cargo")));
        let goenv = match std::env::var_os("GOENV") {
            Some(value) if value == "off" => None,
            Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
            _ => user_config_dir(home.as_deref()).map(|dir| dir.join("go").join("env")),
        };
        UserFiles::read_from(cargo_home.as_deref(), goenv.as_deref())
    }

    /// [`UserFiles::read`] from explicit places.
    pub(crate) fn read_from(cargo_home: Option<&Path>, goenv: Option<&Path>) -> UserFiles {
        let mut files = UserFiles::default();
        if let Some(cargo_home) = cargo_home {
            for name in ["config.toml", "config"] {
                let path = cargo_home.join(name);
                let Some(config) = read_toml(&path) else {
                    continue;
                };
                let cargo = CargoConfig::of(&config);
                if cargo.jobs && files.cargo_jobs.is_none() {
                    files.cargo_jobs = Some(path);
                }
                files.rust_test_threads |= cargo.rust_test_threads;
            }
        }
        if let Some(goenv) = goenv {
            files.goflags = match std::fs::read_to_string(goenv) {
                Ok(text) => GoFlags::Flags(
                    text.lines()
                        .find_map(|line| line.trim().strip_prefix("GOFLAGS="))
                        .unwrap_or_default()
                        .to_owned(),
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    GoFlags::Flags(String::new())
                }
                Err(_) => GoFlags::Unreadable,
            };
        }
        files
    }

    /// The cargo file's keys: each left out where `$CARGO_HOME`'s config
    /// sets it.
    pub(crate) fn cargo_file_keys(&self) -> Keys {
        Keys {
            jobs: self.cargo_jobs.is_none(),
            rust_test_threads: !self.rust_test_threads,
        }
    }
}

/// Go's `os.UserConfigDir`.
fn user_config_dir(home: Option<&Path>) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        return home.map(|home| home.join("Library").join("Application Support"));
    }
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if Path::new(&dir).is_absolute() => Some(PathBuf::from(dir)),
        _ => home.map(|home| home.join(".config")),
    }
}

fn read_small(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn read_toml(path: &Path) -> Option<toml::Table> {
    read_small(path)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn names(computed: &Computed) -> Vec<(&str, &str)> {
        computed
            .variables
            .iter()
            .map(|variable| (variable.name.as_str(), variable.value.as_str()))
            .collect()
    }

    fn inputs<'a>(
        workspace: Option<&'a Path>,
        spec: &'a BuildSpec,
        user: &'a UserFiles,
        make: &'a dyn Fn() -> Make,
    ) -> Inputs<'a> {
        Inputs {
            jobs: 8,
            share: 4,
            jobserver: Ok(Path::new("/state/build/jobserver")),
            make_jobserver: false,
            spec,
            workspace,
            user,
            cargo_file: None,
            make,
        }
    }

    fn no_make() -> Make {
        panic!("make probed without a hazard")
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one case after another, each a few lines"
    )]
    fn variables() {
        let spec = BuildSpec::default();
        let user = UserFiles::default();
        let computed = compute(&inputs(None, &spec, &user, &no_make));
        assert_eq!(computed.skip, JobserverSkip::Unspecified);
        assert_eq!(
            names(&computed),
            [
                (
                    "CARGO_MAKEFLAGS",
                    "-j --jobserver-auth=fifo:/state/build/jobserver"
                ),
                ("CARGO_BUILD_JOBS", "4"),
                ("RUST_TEST_THREADS", "4"),
                ("NEXTEST_TEST_THREADS", "4"),
                ("GOFLAGS", "-p=4"),
            ]
        );

        // The workspace's own settings are respected.
        let temp = tempfile::tempdir().unwrap();
        let ws = temp.path();
        std::fs::create_dir_all(ws.join(".cargo")).unwrap();
        std::fs::create_dir_all(ws.join(".config")).unwrap();
        std::fs::write(
            ws.join(".cargo/config.toml"),
            "[build]\njobs = 3\n[env]\nRUST_TEST_THREADS = \"1\"\n",
        )
        .unwrap();
        std::fs::write(
            ws.join(".config/nextest.toml"),
            "[profile.ci]\ntest-threads = 1\n",
        )
        .unwrap();
        let computed = compute(&inputs(Some(ws), &spec, &user, &no_make));
        assert_eq!(computed.skip, JobserverSkip::CargoJobsSet);
        assert!(
            computed.note.contains(".cargo/config.toml"),
            "{}",
            computed.note
        );
        assert_eq!(names(&computed), [("GOFLAGS", "-p=4")]);

        // The go environment file's flags are kept; a -p there wins.
        let goenv = temp.path().join("goenv");
        std::fs::write(&goenv, "GOFLAGS=-trimpath\nGOPROXY=direct\n").unwrap();
        let user = UserFiles::read_from(None, Some(&goenv));
        let computed = compute(&inputs(None, &spec, &user, &no_make));
        let goflags = &computed.variables[4];
        assert_eq!(goflags.value, "-trimpath -p=4");
        assert_eq!(goflags.merge, i32::from(BuildVariableMerge::Goflags));
        std::fs::write(&goenv, "GOFLAGS=-p=1\n").unwrap();
        let user = UserFiles::read_from(None, Some(&goenv));
        let computed = compute(&inputs(None, &spec, &user, &no_make));
        assert!(
            names(&computed).contains(&("GOFLAGS", "")),
            "sent, empty: only an environment GOFLAGS gets -p"
        );

        // $CARGO_HOME's settings are respected too.
        let cargo_home = temp.path().join("cargo-home");
        std::fs::create_dir_all(&cargo_home).unwrap();
        std::fs::write(
            cargo_home.join("config.toml"),
            "[env]\nRUST_TEST_THREADS = \"2\"\n",
        )
        .unwrap();
        let user = UserFiles::read_from(Some(&cargo_home), None);
        assert_eq!(
            user.cargo_file_keys(),
            Keys {
                jobs: true,
                rust_test_threads: false
            }
        );
        let computed = compute(&inputs(None, &spec, &user, &no_make));
        assert!(
            !names(&computed)
                .iter()
                .any(|(name, _)| *name == "RUST_TEST_THREADS")
        );

        // Under the cargo file, its keys replace the variables.
        let user = UserFiles::default();
        let mut under_file = inputs(None, &spec, &user, &no_make);
        under_file.cargo_file = Some(Keys {
            jobs: true,
            rust_test_threads: true,
        });
        let names_under = names(&compute(&under_file))
            .into_iter()
            .map(|(name, _)| name.to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            names_under,
            ["CARGO_MAKEFLAGS", "NEXTEST_TEST_THREADS", "GOFLAGS"]
        );

        // make_jobserver adds MAKEFLAGS; jobs_env adds the declared names.
        let spec = BuildSpec {
            jobs_env: BTreeMap::from([
                ("VITEST_MAX_WORKERS".to_owned(), "{share}".to_owned()),
                ("CMAKE_BUILD_PARALLEL_LEVEL".to_owned(), "{jobs}".to_owned()),
            ]),
            jobserver: true,
        };
        let mut with_make = inputs(None, &spec, &user, &no_make);
        with_make.make_jobserver = true;
        let computed = compute(&with_make);
        let all = names(&computed);
        assert_eq!(all[1].0, "MAKEFLAGS");
        assert!(all.contains(&("VITEST_MAX_WORKERS", "4")));
        assert!(all.contains(&("CMAKE_BUILD_PARALLEL_LEVEL", "8")));
    }

    #[test]
    fn jobserver_skips() {
        let user = UserFiles::default();
        let spec = BuildSpec {
            jobserver: false,
            ..BuildSpec::default()
        };
        let computed = compute(&inputs(None, &spec, &user, &no_make));
        assert_eq!(computed.skip, JobserverSkip::Repository);
        assert_eq!(computed.variables[0].name, "CARGO_BUILD_JOBS");

        let spec = BuildSpec::default();
        let mut unavailable = inputs(None, &spec, &user, &no_make);
        unavailable.jobserver = Err((JobserverSkip::Unavailable, "mkfifo failed".to_owned()));
        let computed = compute(&unavailable);
        assert_eq!(computed.skip, JobserverSkip::Unavailable);
        assert_eq!(computed.note, "mkfifo failed");
    }

    #[test]
    fn make_hazard() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("Cargo.lock"),
            "[[package]]\nname = \"cmake\"\nversion = \"0.1.58\"\n",
        )
        .unwrap();
        let spec = BuildSpec::default();
        let user = UserFiles::default();
        let found = |modern, present, name: &str| make::Found {
            modern,
            present,
            text: name.to_owned(),
        };
        let old = || Make {
            make: found(false, true, "make 3.81"),
            gmake: found(false, false, "no gmake"),
        };
        let computed = compute(&inputs(Some(temp.path()), &spec, &user, &old));
        assert_eq!(computed.skip, JobserverSkip::MakeTooOld);
        assert!(computed.note.contains("cmake"), "{}", computed.note);
        assert!(computed.note.contains("make 3.81"), "{}", computed.note);
        assert_eq!(computed.variables[0].name, "CARGO_BUILD_JOBS");
        let modern = || Make {
            make: found(false, true, "make 3.81"),
            gmake: found(true, true, "gmake 4.4"),
        };
        let computed = compute(&inputs(Some(temp.path()), &spec, &user, &modern));
        assert_eq!(computed.skip, JobserverSkip::Unspecified);
        assert_eq!(computed.variables[0].name, "CARGO_MAKEFLAGS");
    }

    #[test]
    fn p_flags() {
        assert!(has_p_flag("-p=2"));
        assert!(has_p_flag("-trimpath -p 3"));
        assert!(has_p_flag("--p=1"));
        assert!(!has_p_flag("-trimpath -pgo=auto"));
    }
}
