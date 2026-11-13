//! Which `make` the machine has (#271, ADR 0074): a jobserver named by a
//! path (`fifo:`) reaches `make` only through a build script that passes
//! cargo's `CARGO_MAKEFLAGS` on as `MAKEFLAGS`, and GNU make before 4.4
//! cannot read it (it fails on cargo's `--jobserver-fds=fifo:` spelling;
//! macOS's `/usr/bin/make` is 3.81).
//!
//! Probed only when needed, and never through the `/usr/bin` stub a Mac
//! without the command-line tools has (it offers to install them); cached
//! for [`PROBE_TTL`]. The probe reads the daemon's `PATH`, which can differ
//! from a command's.

use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long a probe's answer is kept.
pub(crate) const PROBE_TTL: Duration = Duration::from_mins(10);

/// How long one `--version` may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Packages known to hand `CARGO_MAKEFLAGS` to `make` as `MAKEFLAGS`, read
/// at the versions in the comments (2026-11-12): a `Cargo.lock` with one of
/// them builds a `make` project under the jobserver.
pub(crate) const MAKE_HAZARDS: [&str; 4] = [
    // 0.1.58: on macOS only when the jobserver is a named pipe.
    "cmake",
    // 400.0.2+4.0.3: `build_libs`, every Unix.
    "openssl-src",
    // 0.2.7: with `NUM_JOBS`, every Unix but the BSDs.
    "autotools",
    // 0.7.1: prepended to `MAKEFLAGS`.
    "tikv-jemalloc-sys",
];

/// One program the probe looked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    /// It is GNU make 4.4 or later.
    pub modern: bool,
    /// It is on `PATH` and answered.
    pub present: bool,
    /// What was found, for a note: `make 3.81`, `no gmake`.
    pub text: String,
}

/// What the probe found on the daemon's `PATH`: `make`, which build
/// scripts run, and `gmake`, which the `cmake` tool's Makefile generator looks for
/// first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Make {
    /// `make`.
    pub make: Found,
    /// `gmake`.
    pub gmake: Found,
}

impl Make {
    /// The program `package`'s build hands the jobserver to: the `cmake`
    /// tool's generator picks `gmake` when there is one, the others run `make`.
    pub(crate) fn for_package(&self, package: &str) -> &Found {
        if package == "cmake" && self.gmake.present {
            &self.gmake
        } else {
            &self.make
        }
    }
}

/// The cached probe.
#[derive(Debug, Default)]
pub(crate) struct MakeProbe {
    cached: Mutex<Option<(Instant, Make)>>,
}

impl MakeProbe {
    /// The machine's `make` and `gmake`, probed at most once per
    /// [`PROBE_TTL`].
    pub(crate) fn get(&self) -> Make {
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, make)) = cached.as_ref()
            && at.elapsed() < PROBE_TTL
        {
            return make.clone();
        }
        let make = Make {
            make: probe_program("make"),
            gmake: probe_program("gmake"),
        };
        *cached = Some((Instant::now(), make.clone()));
        make
    }
}

fn probe_program(program: &str) -> Found {
    let Some(path) = on_path(program) else {
        return Found {
            modern: false,
            present: false,
            text: format!("no {program}"),
        };
    };
    if is_tools_shim(&path) {
        return Found {
            modern: false,
            present: false,
            text: format!("{program} (the command-line tools are not installed)"),
        };
    }
    match version_of(&path.to_string_lossy()) {
        Version::Missing => Found {
            modern: false,
            present: false,
            text: format!("no {program}"),
        },
        Version::NotGnu => Found {
            modern: false,
            present: true,
            text: format!("{program} (not GNU make)"),
        },
        Version::Gnu(major, minor) => Found {
            modern: (major, minor) >= (4, 4),
            present: true,
            text: format!("{program} {major}.{minor}"),
        },
    }
}

/// `program`'s path on the daemon's `PATH`, or `program` itself when it
/// names a path.
fn on_path(program: &str) -> Option<std::path::PathBuf> {
    if program.contains('/') {
        return Some(program.into());
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Whether `path` is macOS's `/usr/bin` stub for a developer tool on a Mac
/// without the command-line tools, which offers to install them when run.
fn is_tools_shim(path: &std::path::Path) -> bool {
    cfg!(target_os = "macos")
        && path.starts_with("/usr/bin")
        && !std::path::Path::new("/Library/Developer/CommandLineTools/usr/bin").is_dir()
        && !Command::new("/usr/bin/xcode-select")
            .arg("-p")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

/// What `program --version` said.
enum Version {
    /// Not on `PATH`, or no answer.
    Missing,
    /// It answered, but not as GNU make.
    NotGnu,
    /// GNU make `(major, minor)`.
    Gnu(u32, u32),
}

/// `program`'s version.
fn version_of(program: &str) -> Version {
    match answer(program) {
        None => Version::Missing,
        Some(text) => match parse_version(&text) {
            Some((major, minor)) => Version::Gnu(major, minor),
            None => Version::NotGnu,
        },
    }
}

/// `program --version`'s stdout, or `None` when it is not on `PATH`, fails
/// or does not answer in time.
fn answer(program: &str) -> Option<String> {
    let mut child = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < PROBE_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `(major, minor)` from `GNU Make 4.4.1`'s first line.
fn parse_version(text: &str) -> Option<(u32, u32)> {
    let rest = text.lines().next()?.trim().strip_prefix("GNU Make ")?;
    let mut parts = rest.split(['.', ' ']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// The first of [`MAKE_HAZARDS`] a `Cargo.lock`'s text lists.
pub(crate) fn hazard_in_lockfile(text: &str) -> Option<&'static str> {
    text.lines().find_map(|line| {
        let name = line.trim().strip_prefix("name = \"")?.strip_suffix('"')?;
        MAKE_HAZARDS.iter().find(|hazard| **hazard == name).copied()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("GNU Make 3.81\nCopyright"), Some((3, 81)));
        assert_eq!(parse_version("GNU Make 4.4.1\n"), Some((4, 4)));
        assert_eq!(parse_version("GNU Make 4.3\n"), Some((4, 3)));
        assert_eq!(parse_version("bmake 2024\n"), None);
    }

    #[test]
    fn probes_a_program() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let fake = |name: &str, version: &str| {
            let path = temp.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().into_owned()
        };
        let old = probe_program(&fake("old", "GNU Make 3.81"));
        assert!(!old.modern && old.present);
        assert!(old.text.ends_with("old 3.81"), "{}", old.text);
        assert!(probe_program(&fake("new", "GNU Make 4.4.1")).modern);
        let bsd = probe_program(&fake("bsd", "bmake"));
        assert!(!bsd.modern && bsd.present);
        let none = probe_program(&temp.path().join("none").to_string_lossy());
        assert!(!none.present, "{none:?}");
    }

    #[test]
    fn cmake_prefers_gmake() {
        let found = |modern, present, name: &str| Found {
            modern,
            present,
            text: name.to_owned(),
        };
        let both = Make {
            make: found(false, true, "make 3.81"),
            gmake: found(true, true, "gmake 4.4"),
        };
        assert!(both.for_package("cmake").modern);
        assert!(!both.for_package("openssl-src").modern);
        let no_gmake = Make {
            make: found(false, true, "make 3.81"),
            gmake: found(false, false, "no gmake"),
        };
        assert_eq!(no_gmake.for_package("cmake").text, "make 3.81");
    }

    #[test]
    fn lockfile_hazards() {
        let lock = "[[package]]\nname = \"libgit2-sys\"\n\n[[package]]\nname = \"cmake\"\nversion = \"0.1.58\"\n";
        assert_eq!(hazard_in_lockfile(lock), Some("cmake"));
        assert_eq!(hazard_in_lockfile("name = \"cmake-ish\"\n"), None);
        assert_eq!(
            hazard_in_lockfile("name = \"openssl-src\"\n"),
            Some("openssl-src")
        );
    }
}
