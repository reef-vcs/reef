//! The sandbox a check runs in (ADR 0060 decision 5): Seatbelt
//! (`sandbox-exec`) on macOS, bubblewrap (`bwrap`) on Linux, or none.
//!
//! A [`Policy`] says what the process may see and write; a [`Backend`]
//! turns it and an argv into a [`std::process::Command`]. Neither knows
//! about jobs, so `reef ws run --sandbox` (#62) can use them.
//!
//! | What | Inside |
//! | --- | --- |
//! | `writable` | read and write |
//! | `readable` | read only, even inside a hidden path |
//! | `hidden` (the home, the executor's work directory) | invisible, except the paths above |
//! | `denied` (Reef's state and configuration, the socket, the runtime directory) | invisible, even inside a readable path |
//! | everything else | read only |
//! | network | per [`Network`] |
//! | other processes | not visible (Linux), not signalled or inspected (macOS) |
//! | environment | exactly `env` |

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;
use crate::config::{Network, SandboxKind, SandboxSection};

/// What a sandboxed process may see and do.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    /// The working directory; inside a writable path.
    pub cwd: PathBuf,
    /// Read and write.
    pub writable: Vec<PathBuf>,
    /// Read only, even inside a hidden path.
    pub readable: Vec<PathBuf>,
    /// Invisible, except the readable and writable paths inside them.
    pub hidden: Vec<PathBuf>,
    /// Invisible, even inside a readable path. Directories, files or
    /// sockets.
    pub denied: Vec<PathBuf>,
    /// The network.
    pub network: Network,
    /// The whole environment.
    pub env: Vec<(OsString, OsString)>,
}

/// A sandbox implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backend {
    /// `sandbox-exec` with a generated profile.
    Seatbelt {
        /// `sandbox-exec`'s path.
        program: PathBuf,
    },
    /// `bwrap` with generated arguments.
    Bubblewrap {
        /// `bwrap`'s path.
        program: PathBuf,
    },
    /// A plain child process.
    None,
}

/// The `sandbox-exec` every macOS release ships.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

impl Backend {
    /// The backend `[sandbox]` asks for, if its program exists. Whether it
    /// applies is [`Backend::self_test`]'s question.
    pub fn resolve(section: &SandboxSection) -> Result<Backend, Error> {
        let kind = match section.kind {
            SandboxKind::Auto if cfg!(target_os = "macos") => SandboxKind::Seatbelt,
            SandboxKind::Auto if cfg!(target_os = "linux") => SandboxKind::Bubblewrap,
            SandboxKind::Auto => {
                return Err(unavailable(
                    &format!(
                        "no sandbox exists for {} in this executor",
                        std::env::consts::OS
                    ),
                    "Run checks on macOS or Linux, or set [sandbox] kind = \"none\" in executor.toml if this machine is already isolated (a fresh VM or container per run).",
                ));
            }
            kind => kind,
        };
        match kind {
            SandboxKind::Seatbelt => {
                if !cfg!(target_os = "macos") {
                    return Err(unavailable(
                        "Seatbelt is macOS only",
                        "Use kind = \"bubblewrap\" on Linux, or \"auto\".",
                    ));
                }
                let program = PathBuf::from(SANDBOX_EXEC);
                if !program.exists() {
                    return Err(unavailable(
                        "/usr/bin/sandbox-exec is missing on this macOS",
                        "This macOS has no Seatbelt command line; run checks in a VM and set [sandbox] kind = \"none\".",
                    ));
                }
                Ok(Backend::Seatbelt { program })
            }
            SandboxKind::Bubblewrap => {
                if !cfg!(target_os = "linux") {
                    return Err(unavailable(
                        "bubblewrap is Linux only",
                        "Use kind = \"seatbelt\" on macOS, or \"auto\".",
                    ));
                }
                let program = if section.bwrap.is_empty() {
                    find_on_path("bwrap").ok_or_else(|| {
                        unavailable(
                            "bwrap is not on PATH",
                            "Install bubblewrap 0.8 or later (`apt-get install bubblewrap`, `dnf install bubblewrap`), or name it with [sandbox] bwrap.",
                        )
                    })?
                } else {
                    let program = PathBuf::from(&section.bwrap);
                    if !program.exists() {
                        return Err(unavailable(
                            &format!("[sandbox] bwrap = {:?} does not exist", section.bwrap),
                            "Install bubblewrap 0.8 or later, or fix [sandbox] bwrap.",
                        ));
                    }
                    program
                };
                Ok(Backend::Bubblewrap { program })
            }
            SandboxKind::None | SandboxKind::Auto => Ok(Backend::None),
        }
    }

    /// The kind's name in labels and annotations: `seatbelt`,
    /// `bubblewrap` or `none`.
    pub fn kind(&self) -> &'static str {
        match self {
            Backend::Seatbelt { .. } => "seatbelt",
            Backend::Bubblewrap { .. } => "bubblewrap",
            Backend::None => "none",
        }
    }

    /// The command that runs `argv` under `policy`. The caller adds stdio
    /// and spawns it.
    pub fn command(&self, policy: &Policy, argv: &[String]) -> Result<Command, Error> {
        let (program, rest) = argv.split_first().ok_or_else(|| Error::Config {
            message: "an empty argv".into(),
        })?;
        let mut command = match self {
            Backend::None => {
                let mut command = Command::new(program);
                command.args(rest);
                command
            }
            Backend::Seatbelt { program: exec } => {
                let profile = seatbelt_profile(policy)?;
                let mut command = Command::new(exec);
                command
                    .arg("-p")
                    .arg(profile)
                    .arg("--")
                    .arg(program)
                    .args(rest);
                command
            }
            Backend::Bubblewrap { program: bwrap } => {
                // bwrap itself runs with no environment: a variable such as
                // LD_PRELOAD would load the tree's code into it before any
                // namespace exists. The check's environment is bwrap's to
                // set, inside.
                let mut command = Command::new(bwrap);
                command.env_clear();
                command.args(bwrap_args(policy));
                command.arg("--clearenv");
                for (name, value) in &policy.env {
                    command.arg("--setenv").arg(name).arg(value);
                }
                command.arg("--").arg(program).args(rest);
                command.current_dir(&policy.cwd);
                return Ok(command);
            }
        };
        command.current_dir(&policy.cwd);
        command.env_clear();
        command.envs(policy.env.iter().map(|(k, v)| (k, v)));
        Ok(command)
    }

    /// Whether this backend applies on this machine: runs `true` under a
    /// small policy. The error says why not and how to fix it.
    pub fn self_test(&self) -> Result<(), Error> {
        let io = |source| Error::Io {
            context: "a directory for the sandbox's self-test".into(),
            source,
        };
        let dir = tempfile::tempdir().map_err(io)?;
        let cwd = dir.path().canonicalize().map_err(io)?;
        let policy = Policy {
            cwd: cwd.clone(),
            writable: vec![cwd],
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            ..Policy::default()
        };
        let truth = if Path::new("/usr/bin/true").exists() {
            "/usr/bin/true"
        } else {
            "/bin/true"
        };
        let output = self
            .command(&policy, &[truth.to_owned()])?
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|source| Error::Io {
                context: format!("starting {}", self.kind()),
                source,
            })?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(self.diagnose(&stderr))
    }

    /// The unavailable error for a backend that failed with `stderr`.
    fn diagnose(&self, stderr: &str) -> Error {
        match self {
            Backend::Seatbelt { .. } if stderr.contains("sandbox_apply") => unavailable(
                &format!("Seatbelt cannot be applied here: {stderr}"),
                "This process is already inside a Seatbelt sandbox (a CI runner's, or a harness's), and macOS refuses a second one. Run the executor outside it, or set [sandbox] kind = \"none\" if the outer sandbox is the isolation you want.",
            ),
            Backend::Bubblewrap { .. }
                if stderr.contains("uid map")
                    || stderr.contains("No permissions to create new namespace")
                    || stderr.contains("Operation not permitted") =>
            {
                unavailable(
                    &format!("bubblewrap cannot create a user namespace: {stderr}"),
                    "Unprivileged user namespaces are restricted. On Ubuntu 24.04 and later, install an AppArmor profile for bwrap that allows `userns` (docs/executor.md has one), or set `sysctl kernel.apparmor_restrict_unprivileged_userns=0`; in a container, allow unshare (for example --privileged).",
                )
            }
            _ => unavailable(
                &format!("{} failed its self-test: {stderr}", self.kind()),
                "Run `reef executor check` for the probes, or set [sandbox] kind = \"none\" if this machine is already isolated.",
            ),
        }
    }
}

fn unavailable(message: &str, hint: &str) -> Error {
    Error::SandboxUnavailable {
        message: message.to_owned(),
        hint: hint.to_owned(),
    }
}

/// `program` on the executor's `PATH`.
pub fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    find_in(program, &path)
}

/// `program` in the directories of `path` (a `PATH` value), or `program`
/// itself when it has a slash; executable files only.
pub fn find_in(program: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let executable = |candidate: &Path| {
        candidate
            .metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        let candidate = PathBuf::from(program);
        return executable(&candidate).then_some(candidate);
    }
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|candidate| executable(candidate))
}

/// `path` as the kernel names it: canonical when it exists, its parent's
/// canonical form joined with its name when only the parent does. Seatbelt
/// matches real paths (`/var` is `/private/var` on macOS).
pub fn real_path(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => real_path(parent).join(name),
        _ => path.to_owned(),
    }
}

/// An SBPL string literal for `path`. Control characters are refused.
fn sbpl_string(path: &Path) -> Result<String, Error> {
    let text = path.to_str().ok_or_else(|| Error::Config {
        message: format!(
            "{} is not UTF-8; Seatbelt profiles need UTF-8 paths",
            path.display()
        ),
    })?;
    if text.chars().any(char::is_control) {
        return Err(Error::Config {
            message: format!("{text:?} has a control character"),
        });
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Ok(out)
}

/// The Seatbelt profile for `policy`. Later rules override earlier ones.
pub fn seatbelt_profile(policy: &Policy) -> Result<String, Error> {
    use std::fmt::Write as _;
    let real = |paths: &[PathBuf]| -> Vec<PathBuf> { paths.iter().map(|p| real_path(p)).collect() };
    let writable = real(&policy.writable);
    let readable = real(&policy.readable);
    let hidden = real(&policy.hidden);
    let denied = real(&policy.denied);
    let subpaths = |paths: &[PathBuf]| -> Result<String, Error> {
        paths
            .iter()
            .map(|p| Ok(format!(" (subpath {})", sbpl_string(p)?)))
            .collect()
    };
    let mut profile = String::from("(version 1)\n(allow default)\n");
    match policy.network {
        Network::None => profile.push_str("(deny network*)\n"),
        Network::Loopback => profile.push_str(
            "(deny network*)\n(allow network* (remote ip \"localhost:*\"))\n(allow network* (local ip \"localhost:*\"))\n",
        ),
        // The host's network, but no Unix socket but the resolver's: the
        // user's agents (ssh-agent, gpg-agent) and services listen on them.
        Network::Full => profile.push_str(
            "(deny network-outbound (remote unix-socket))\n(allow network-outbound (remote unix-socket (path-literal \"/private/var/run/mDNSResponder\")))\n",
        ),
    }
    // Writes: only the writable paths, and the devices every program uses.
    profile.push_str("(deny file-write*)\n(allow file-write*");
    profile.push_str(&subpaths(&writable)?);
    profile.push_str(
        " (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/dtracehelper\") (regex #\"^/dev/fd/\") (literal \"/dev/tty\"))\n",
    );
    // Reads: the hidden paths refused, then the readable and writable paths
    // inside them allowed again, with their ancestors statable (getcwd and
    // path walks need that).
    if !hidden.is_empty() {
        profile.push_str("(deny file-read*");
        profile.push_str(&subpaths(&hidden)?);
        profile.push_str(")\n(allow file-read*");
        profile.push_str(&subpaths(&readable)?);
        profile.push_str(&subpaths(&writable)?);
        profile.push_str(")\n");
        let mut ancestors: Vec<PathBuf> = readable
            .iter()
            .chain(&writable)
            .flat_map(|p| {
                p.ancestors()
                    .skip(1)
                    .map(Path::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect();
        ancestors.sort();
        ancestors.dedup();
        if !ancestors.is_empty() {
            profile.push_str("(allow file-read-metadata");
            for ancestor in &ancestors {
                let _ = write!(profile, " (literal {})", sbpl_string(ancestor)?);
            }
            profile.push_str(")\n");
        }
    }
    // Denied: last, so nothing above reopens them. A Unix socket is
    // connected to by path, which the file rules do not govern.
    if !denied.is_empty() {
        profile.push_str("(deny file-read* file-write*");
        for path in &denied {
            let _ = write!(profile, " (subpath {0}) (literal {0})", sbpl_string(path)?);
        }
        profile.push_str(")\n");
        if policy.network != Network::None {
            profile.push_str("(deny network-outbound");
            for path in &denied {
                let _ = write!(
                    profile,
                    " (remote unix-socket (subpath {}))",
                    sbpl_string(path)?
                );
            }
            profile.push_str(")\n");
        }
    }
    // Other processes: not signalled, not inspected. The keychain is not
    // reachable.
    profile.push_str(
        "(deny signal)\n(allow signal (target same-sandbox))\n(deny process-info*)\n(allow process-info* (target same-sandbox))\n(deny mach-lookup (global-name \"com.apple.SecurityServer\") (global-name \"com.apple.securityd\"))\n",
    );
    Ok(profile)
}

/// bubblewrap's arguments for `policy`, before `--` and the argv.
pub fn bwrap_args(policy: &Policy) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--die-with-parent",
        "--new-session",
        "--unshare-user",
        "--unshare-pid",
        // The check is the namespace's PID 1, which bwrap reaps itself, so
        // its CPU time reaches the executor's wait4 (with bwrap's own init,
        // bwrap exits without reaping it). Killing bwrap still takes the
        // namespace: the kernel ends every process in it when PID 1 dies.
        "--as-pid-1",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-cgroup-try",
        "--cap-drop",
        "ALL",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    if policy.network != Network::Full {
        args.push("--unshare-net".into());
    }
    let mut push = |items: &[&std::ffi::OsStr]| args.extend(items.iter().map(OsString::from));
    push(&["--ro-bind".as_ref(), "/".as_ref(), "/".as_ref()]);
    push(&["--dev".as_ref(), "/dev".as_ref()]);
    push(&["--proc".as_ref(), "/proc".as_ref()]);
    for scratch in ["/tmp", "/var/tmp", "/run"] {
        push(&["--tmpfs".as_ref(), scratch.as_ref()]);
    }
    if policy.network == Network::Full && Path::new("/run/systemd/resolve").is_dir() {
        push(&[
            "--ro-bind".as_ref(),
            "/run/systemd/resolve".as_ref(),
            "/run/systemd/resolve".as_ref(),
        ]);
    }
    let mut hidden = policy.hidden.clone();
    hidden.sort_by_key(|p| p.components().count());
    for path in &hidden {
        if path.is_dir() {
            push(&["--tmpfs".as_ref(), path.as_os_str()]);
        }
    }
    for path in &policy.readable {
        push(&["--ro-bind-try".as_ref(), path.as_os_str(), path.as_os_str()]);
    }
    for path in &policy.denied {
        if path.is_dir() {
            push(&["--tmpfs".as_ref(), path.as_os_str()]);
        } else if path.exists() {
            push(&["--ro-bind".as_ref(), "/dev/null".as_ref(), path.as_os_str()]);
        }
    }
    for path in &policy.writable {
        push(&["--bind".as_ref(), path.as_os_str(), path.as_os_str()]);
    }
    push(&["--chdir".as_ref(), policy.cwd.as_os_str()]);
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy {
            cwd: "/zz-reef/w/jobs/1/tree".into(),
            writable: vec![
                "/zz-reef/w/jobs/1/tree".into(),
                "/zz-reef/w/jobs/1/tmp".into(),
            ],
            readable: vec!["/zz-reef/home/u/.cargo".into()],
            hidden: vec!["/zz-reef/home/u".into(), "/zz-reef/w".into()],
            denied: vec!["/zz-reef/home/u/.local/state/reef".into()],
            network: Network::None,
            env: vec![("PATH".into(), "/bin".into())],
        }
    }

    #[test]
    fn profiles_deny_the_network_writes_and_hidden_reads() {
        let profile = seatbelt_profile(&policy()).unwrap();
        assert!(profile.contains("(deny network*)"), "{profile}");
        assert!(profile.contains("(deny file-write*)"), "{profile}");
        assert!(
            profile.contains("(subpath \"/zz-reef/w/jobs/1/tree\")"),
            "{profile}"
        );
        assert!(
            profile.contains("(deny file-read* (subpath \"/zz-reef/home/u\")"),
            "{profile}"
        );
        assert!(
            profile.contains("(literal \"/zz-reef/home/u\")"),
            "{profile}"
        );
        assert!(
            profile.contains("(allow signal (target same-sandbox))"),
            "{profile}"
        );
        let deny = profile.find("(deny file-read* file-write*").unwrap();
        let allow = profile.find("(allow file-read*").unwrap();
        assert!(deny > allow, "denied paths come last:\n{profile}");
    }

    #[test]
    fn profile_strings_are_escaped_and_control_characters_refused() {
        assert_eq!(
            sbpl_string(Path::new("/a\"b\\c")).unwrap(),
            "\"/a\\\"b\\\\c\""
        );
        assert!(sbpl_string(Path::new("/a\nb")).is_err());
    }

    #[test]
    fn bwrap_hides_then_rebinds() {
        let args: Vec<String> = bwrap_args(&policy())
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        let at = |needle: &str| args.iter().position(|a| a == needle).unwrap();
        assert!(args.contains(&"--unshare-net".to_owned()));
        assert!(at("--ro-bind-try") > at("--tmpfs"));
        assert_eq!(args.last().unwrap(), "/zz-reef/w/jobs/1/tree");
        let full = Policy {
            network: Network::Full,
            ..policy()
        };
        assert!(!bwrap_args(&full).contains(&"--unshare-net".into()));
    }

    #[test]
    fn bwrap_runs_with_no_environment_of_its_own() {
        let mut policy = policy();
        policy.env.push(("LD_PRELOAD".into(), "./evil.so".into()));
        let command = Backend::Bubblewrap {
            program: "/usr/bin/bwrap".into(),
        }
        .command(&policy, &["true".to_owned()])
        .unwrap();
        let envs: Vec<_> = command.get_envs().collect();
        assert!(
            envs.iter().all(|(_, value)| value.is_none()),
            "bwrap's own environment is cleared: {envs:?}"
        );
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let at = args.iter().position(|a| a == "LD_PRELOAD").unwrap();
        assert_eq!(args[at - 1], "--setenv");
        assert!(args.contains(&"--clearenv".to_owned()));
    }

    #[test]
    fn full_network_reaches_no_host_unix_socket() {
        let full = Policy {
            network: Network::Full,
            ..policy()
        };
        let profile = seatbelt_profile(&full).unwrap();
        assert!(
            profile.contains("(deny network-outbound (remote unix-socket))"),
            "{profile}"
        );
        assert!(!profile.contains("^/dev/tty"), "{profile}");
    }

    #[test]
    fn programs_are_found_on_the_check_s_path() {
        assert!(find_in("sh", "/usr/bin:/bin".as_ref()).is_some());
        assert!(find_in("no-such-program-here", "/usr/bin:/bin".as_ref()).is_none());
        assert!(find_in("/bin/sh", "".as_ref()).is_some());
    }
}
