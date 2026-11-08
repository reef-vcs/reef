//! Escape probes (S-82, S-83, S-87): each tries one thing the sandbox
//! promises to refuse, run as `reef executor __probe <name> <arg>` inside
//! the configured sandbox, beside a control run without it.
//!
//! The paths they aim at are a fake home, state directory, sockets and
//! work directory made for the run, plus the real home and Reef's real
//! paths as the executor hides them, so a probe never touches anything of
//! value.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::Error;
use crate::config::Network;
use crate::report::ProbeReport;
use crate::sandbox::{Backend, Policy, real_path};

/// The probe's exit code when the action succeeded: an escape.
pub const ALLOWED: i32 = 0;
/// The probe's exit code when the action was refused.
pub const REFUSED: i32 = 10;

/// Variables a check must never see.
const SECRET_NAMES: &[&str] = &[
    "REEF_REMOTE_TOKEN",
    "REEF_SOCKET",
    "REEF_STATE_DIR",
    "REEF_REMOTE",
    "SSH_AUTH_SOCK",
    "GITHUB_TOKEN",
    "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
    "ACTIONS_RUNTIME_TOKEN",
];

/// Runs one probe in this process (the sandboxed side) and returns its
/// exit code: [`ALLOWED`] or [`REFUSED`], with a line of detail on stdout.
// The probe's detail is its output, read by the executor.
#[allow(clippy::print_stdout)]
pub fn probe_main(name: &str, arg: &str) -> i32 {
    let (allowed, detail) = match name {
        "connect-unix" => match std::os::unix::net::UnixStream::connect(arg) {
            Ok(_) => (true, format!("connected to {arg}")),
            Err(error) => (false, error.to_string()),
        },
        "connect-tcp" => match std::net::TcpStream::connect_timeout(
            &arg.parse().unwrap_or_else(|_| ([127, 0, 0, 1], 9).into()),
            std::time::Duration::from_secs(2),
        ) {
            Ok(_) => (true, format!("connected to {arg}")),
            Err(error) => (false, error.to_string()),
        },
        "read" => match std::fs::read(arg) {
            Ok(bytes) => (true, format!("read {} bytes", bytes.len())),
            Err(error) => (false, error.to_string()),
        },
        "write" => match std::fs::write(arg, b"escaped") {
            Ok(()) => (true, format!("wrote {arg}")),
            Err(error) => (false, error.to_string()),
        },
        "list" => match std::fs::read_dir(arg) {
            Ok(entries) => {
                let names = entries.filter_map(Result::ok).count();
                (names > 0, format!("listed {names} entries"))
            }
            Err(error) => (false, error.to_string()),
        },
        "procargs" => procargs(arg),
        "signal" => {
            let pid = arg.parse().unwrap_or(0);
            match nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGCONT,
            ) {
                Ok(()) => (true, format!("signalled pid {pid}")),
                Err(error) => (false, error.to_string()),
            }
        }
        "setuid" => setuid(),
        "environment" => {
            let found: Vec<String> = std::env::vars_os()
                .filter_map(|(name, _)| name.into_string().ok())
                .filter(|name| {
                    SECRET_NAMES.contains(&name.as_str())
                        || (name.starts_with("REEF_") && !name.starts_with("REEF_CHECK_"))
                        || name.starts_with("PROBE_CANARY")
                })
                .collect();
            if found.is_empty() {
                (false, "no secret variable".into())
            } else {
                (true, format!("saw {}", found.join(", ")))
            }
        }
        _ => {
            println!("unknown probe {name}");
            return 2;
        }
    };
    println!("{detail}");
    if allowed { ALLOWED } else { REFUSED }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn procargs(arg: &str) -> (bool, String) {
    let pid: libc::c_int = arg.parse().unwrap_or(0);
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: a null buffer asks for the size.
    let sized = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if sized != 0 {
        return (false, std::io::Error::last_os_error().to_string());
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: the buffer has `size` bytes.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read == 0 {
        (
            true,
            format!("read {size} bytes of pid {pid}'s arguments and environment"),
        )
    } else {
        (false, std::io::Error::last_os_error().to_string())
    }
}

#[cfg(not(target_os = "macos"))]
fn procargs(arg: &str) -> (bool, String) {
    match std::fs::read(format!("/proc/{arg}/environ")) {
        Ok(bytes) if !bytes.is_empty() => (
            true,
            format!("read {} bytes of /proc/{arg}/environ", bytes.len()),
        ),
        Ok(_) => (
            false,
            format!("/proc/{arg}/environ is empty (not the executor's)"),
        ),
        Err(error) => (false, error.to_string()),
    }
}

#[cfg(target_os = "macos")]
fn setuid() -> (bool, String) {
    // /bin/ps is setuid root on macOS; Seatbelt refuses to run setuid
    // programs.
    match std::process::Command::new("/bin/ps")
        .args(["-p", "1", "-o", "pid="])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) if output.status.success() => (true, "ran setuid /bin/ps".into()),
        Ok(output) => (
            false,
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ),
        Err(error) => (false, error.to_string()),
    }
}

#[cfg(not(target_os = "macos"))]
fn setuid() -> (bool, String) {
    // With no_new_privs, a setuid program does not elevate.
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let set = status
        .lines()
        .any(|line| line.starts_with("NoNewPrivs:") && line.trim_end().ends_with('1'));
    if set {
        (
            false,
            "no_new_privs is set: setuid programs do not elevate".into(),
        )
    } else {
        (true, "no_new_privs is not set".into())
    }
}

/// Paths the probes aim at, made for one run.
struct Bait {
    root: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
    work: PathBuf,
    job: PathBuf,
    outside: PathBuf,
    _state_socket: std::os::unix::net::UnixListener,
    _runtime_socket: std::os::unix::net::UnixListener,
    tcp: std::net::TcpListener,
}

impl Bait {
    fn new() -> std::io::Result<Bait> {
        let root = tempfile::Builder::new().prefix("reef-probe-").tempdir()?;
        let base = real_path(root.path());
        let home = base.join("home");
        let state = base.join("state");
        let runtime = base.join("runtime");
        let work = base.join("work");
        let job = work.join("jobs").join("probe");
        let outside = base.join("outside");
        for dir in [&home, &state, &runtime, &outside] {
            std::fs::create_dir_all(dir)?;
        }
        for sub in ["tree", "home", "tmp"] {
            std::fs::create_dir_all(job.join(sub))?;
        }
        std::fs::create_dir_all(work.join("jobs/other"))?;
        std::fs::create_dir_all(work.join("blobs/aa"))?;
        std::fs::write(home.join("secret"), "home secret")?;
        std::fs::write(state.join("state.db"), "state")?;
        std::fs::write(work.join("jobs/other/secret"), "other job")?;
        std::fs::write(work.join("blobs/aa/bb"), "blob")?;
        let state_socket = std::os::unix::net::UnixListener::bind(state.join("daemon.sock"))?;
        let runtime_socket = std::os::unix::net::UnixListener::bind(runtime.join("reef.sock"))?;
        let tcp = std::net::TcpListener::bind("127.0.0.1:0")?;
        Ok(Bait {
            root,
            home,
            state,
            runtime,
            work,
            job,
            outside,
            _state_socket: state_socket,
            _runtime_socket: runtime_socket,
            tcp,
        })
    }
}

/// One probe: what it is, its argument, and whether this platform's
/// sandbox is expected to refuse it.
struct Probe {
    name: &'static str,
    action: &'static str,
    arg: String,
    tries: &'static str,
    /// For a write: the host path whose existence afterwards says whether
    /// it escaped (a tmpfs inside bubblewrap would take the write).
    host_file: Option<PathBuf>,
    expected: bool,
}

/// What the executor hides around every check, for the probe run.
#[derive(Clone, Debug, Default)]
pub struct Surround {
    /// Readable paths (`[sandbox] read`).
    pub readable: Vec<PathBuf>,
    /// The real home and work directory.
    pub hidden: Vec<PathBuf>,
    /// The real state, configuration, socket and runtime paths.
    pub denied: Vec<PathBuf>,
    /// The environment a check gets (built as a job's).
    pub env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
}

/// Runs every probe inside `backend` and in a control run, and reports
/// each. `exe` is the `reef` binary that runs `executor __probe`.
#[allow(clippy::too_many_lines)]
pub fn run_all(
    backend: &Backend,
    exe: &Path,
    surround: &Surround,
) -> Result<Vec<ProbeReport>, Error> {
    let bait = Bait::new().map_err(|source| Error::Io {
        context: "making the probes' bait".into(),
        source,
    })?;
    let me = std::process::id().to_string();
    let probes = vec![
        Probe {
            name: "state-socket",
            action: "connect-unix",
            arg: bait.state.join("daemon.sock").display().to_string(),
            tries: "connect to a Unix socket in Reef's state directory",
            host_file: None,
            expected: true,
        },
        Probe {
            name: "runtime-socket",
            action: "connect-unix",
            arg: bait.runtime.join("reef.sock").display().to_string(),
            tries: "connect to a socket at REEF_SOCKET's path in the runtime directory",
            host_file: None,
            expected: true,
        },
        Probe {
            name: "host-loopback",
            action: "connect-tcp",
            arg: bait
                .tcp
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or_default(),
            tries: "connect to a TCP listener on the host's loopback (network = none)",
            host_file: None,
            expected: true,
        },
        Probe {
            name: "read-home",
            action: "read",
            arg: bait.home.join("secret").display().to_string(),
            tries: "read a file in the home, outside [sandbox] read",
            host_file: None,
            expected: true,
        },
        Probe {
            name: "read-state",
            action: "read",
            arg: bait.state.join("state.db").display().to_string(),
            tries: "read a file in Reef's state directory",
            host_file: None,
            expected: true,
        },
        Probe {
            name: "read-cache",
            action: "read",
            arg: bait.work.join("blobs/aa/bb").display().to_string(),
            tries: "read the executor's blob cache",
            host_file: None,
            expected: true,
        },
        Probe {
            name: "read-other-job",
            action: "read",
            arg: bait.work.join("jobs/other/secret").display().to_string(),
            tries: "read another job's directory",
            host_file: None,
            expected: true,
        },
        Probe {
            name: "write-home",
            action: "write",
            arg: bait.home.join("written").display().to_string(),
            tries: "write a file in the home",
            host_file: Some(bait.home.join("written")),
            expected: true,
        },
        Probe {
            name: "write-outside",
            action: "write",
            arg: bait.outside.join("written").display().to_string(),
            tries: "write a file outside the job's directory",
            host_file: Some(bait.outside.join("written")),
            expected: true,
        },
        Probe {
            name: "write-work",
            action: "write",
            arg: bait.work.join("written").display().to_string(),
            tries: "write in the executor's work directory",
            host_file: Some(bait.work.join("written")),
            expected: true,
        },
        Probe {
            name: "executor-environment",
            action: "procargs",
            arg: me.clone(),
            tries: "read the executor's arguments and environment",
            host_file: None,
            // macOS has no Seatbelt rule for KERN_PROCARGS2: the executor
            // keeps no secret in its environment instead (docs/executor.md).
            expected: !cfg!(target_os = "macos"),
        },
        Probe {
            name: "signal-executor",
            action: "signal",
            arg: me,
            tries: "signal the executor's process",
            host_file: None,
            expected: true,
        },
        Probe {
            name: "setuid",
            action: "setuid",
            arg: String::new(),
            tries: "run a setuid program with its privileges",
            host_file: None,
            expected: true,
        },
        Probe {
            name: "environment",
            action: "environment",
            arg: String::new(),
            tries: "see a variable of the executor's (a token, a socket) in the check's environment",
            host_file: None,
            expected: true,
        },
    ];
    let job = &bait.job;
    let mut readable = surround.readable.clone();
    readable.push(real_path(exe));
    let mut hidden = surround.hidden.clone();
    hidden.extend([bait.home.clone(), bait.work.clone()]);
    let mut denied = surround.denied.clone();
    denied.extend([bait.state.clone(), bait.runtime.clone()]);
    let policy = Policy {
        cwd: job.join("tree"),
        writable: vec![job.join("tree"), job.join("home"), job.join("tmp")],
        readable,
        hidden,
        denied,
        network: Network::None,
        env: surround.env.clone(),
    };
    let mut reports = Vec::with_capacity(probes.len());
    for probe in &probes {
        let (sandboxed, inside) = run_one(backend, exe, &policy, probe)?;
        let (_, control) = run_one(&Backend::None, exe, &policy, probe)?;
        reports.push(ProbeReport {
            name: probe.name.to_owned(),
            tries: probe.tries.to_owned(),
            boundary: probe.expected,
            refused: sandboxed == Seen::Refused,
            detail: format!("{inside}; without the sandbox: {control}"),
        });
    }
    drop(bait.root);
    Ok(reports)
}

/// Whether the probes held: every probe the platform promises is refused.
pub fn holds(reports: &[ProbeReport]) -> bool {
    reports.iter().all(|r| r.refused || !r.boundary)
}

/// What one probe run saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    Escaped,
    Refused,
    DidNotRun,
}

/// Runs one probe; what it saw, and its detail.
fn run_one(
    backend: &Backend,
    exe: &Path,
    policy: &Policy,
    probe: &Probe,
) -> Result<(Seen, String), Error> {
    if let Some(file) = &probe.host_file {
        let _ = std::fs::remove_file(file);
    }
    let argv = vec![
        exe.display().to_string(),
        "executor".into(),
        "__probe".into(),
        probe.action.into(),
        probe.arg.clone(),
    ];
    let mut command = backend.command(policy, &argv)?;
    // The control run sees the canary; a sandboxed check's environment is
    // the policy's, so it never does.
    if *backend == Backend::None && probe.action == "environment" {
        command.env("PROBE_CANARY", "1");
    }
    let output = command
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| Error::Io {
            context: format!("running probe {}", probe.name),
            source,
        })?;
    let detail = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let mut escaped = match output.status.code() {
        Some(ALLOWED) => true,
        Some(REFUSED) => false,
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Ok((
                Seen::DidNotRun,
                format!(
                    "did not run ({}): {}",
                    output.status,
                    stderr.lines().next().unwrap_or_default()
                ),
            ));
        }
    };
    let mut detail = if detail.is_empty() {
        "no detail".to_owned()
    } else {
        detail
    };
    if let Some(file) = &probe.host_file {
        if escaped && !file.exists() {
            detail.push_str(" (inside the sandbox only: the host's file was never written)");
        }
        escaped = file.exists();
        let _ = std::fs::remove_file(file);
    }
    let seen = if escaped {
        Seen::Escaped
    } else {
        Seen::Refused
    };
    Ok((seen, detail))
}
