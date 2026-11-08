//! Running one check as a process: its own session, its output to a capped
//! log, its CPU time from `wait4`, and every process it left killed when it
//! ends.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// Why the executor stopped a check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The check's own `timeout`.
    Timeout,
    /// The job's wall time.
    Budget,
    /// The job was cancelled or its claim lost.
    Cancelled,
    /// The executor is shutting down.
    Shutdown,
}

impl StopReason {
    /// The name in the annotations' `stopped`.
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::Timeout => "timeout",
            StopReason::Budget => "budget",
            StopReason::Cancelled => "cancelled",
            StopReason::Shutdown => "shutdown",
        }
    }
}

/// How a check's process ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended {
    /// It exited with this code.
    Exited(i32),
    /// A signal the executor did not send killed it.
    Signaled(i32),
    /// The executor stopped it.
    Stopped(StopReason),
    /// It never ran: the program was missing, or the sandbox refused it.
    NotStarted(String),
}

/// What running a check produced.
#[derive(Clone, Debug)]
pub struct Ran {
    /// How it ended.
    pub ended: Ended,
    /// From spawn to exit.
    pub wall: Duration,
    /// User and system time of it and the children it waited for.
    pub cpu: Duration,
    /// Bytes written to the log.
    pub logged: u64,
    /// Bytes dropped past the cap.
    pub dropped: u64,
}

/// How to run a check.
pub struct Spec<'a> {
    /// The sandboxed command, environment and directory set.
    pub command: Command,
    /// The program, to check that it exists on the check's `PATH` first.
    pub program: &'a str,
    /// The check's `PATH`.
    pub path: &'a std::ffi::OsStr,
    /// The log file, created here.
    pub log: &'a Path,
    /// The most bytes kept.
    pub max_log: u64,
    /// When the check's timeout or the job's budget runs out, and which.
    pub deadline: Option<(tokio::time::Instant, StopReason)>,
    /// Becomes `Some` when the job is cancelled or the executor stops.
    pub stop: watch::Receiver<Option<StopReason>>,
    /// Between `SIGTERM` and `SIGKILL`.
    pub kill_grace: Duration,
    /// Processes whose working directory is under this are killed when the
    /// check ends.
    pub sweep: PathBuf,
    /// Whether the command is `sandbox-exec` (its own failures are
    /// recognized).
    pub seatbelt: bool,
}

/// Runs the check to its end.
#[allow(clippy::too_many_lines)]
pub async fn run(mut spec: Spec<'_>) -> std::io::Result<Ran> {
    let started = Instant::now();
    let mut log = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(spec.log)?;
    if spec.program.contains('/') {
        // A path: inside the sandbox it is the same path.
        if crate::sandbox::find_in(spec.program, "".as_ref()).is_none() {
            let message = format!("{} does not exist or is not executable", spec.program);
            writeln!(log, "reef executor: {message}")?;
            return Ok(not_started(message, started));
        }
    } else if crate::sandbox::find_in(spec.program, spec.path).is_none() {
        let message = format!("{} is not on the check's PATH", spec.program);
        writeln!(log, "reef executor: {message}")?;
        return Ok(not_started(message, started));
    }
    let (reader, writer) = std::io::pipe()?;
    spec.command
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer);
    new_session(&mut spec.command);
    let child = match spec.command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let message = format!("could not start: {error}");
            writeln!(log, "reef executor: {message}")?;
            return Ok(not_started(message, started));
        }
    };
    // The pipe's write ends are the child's now.
    drop(spec.command);
    let pid = i32::try_from(child.id()).unwrap_or(i32::MAX);
    // Reaped by wait4 below, not by `Child`.
    drop(child);
    let max_log = spec.max_log;
    let logger = std::thread::spawn(move || copy_capped(reader, &mut log, max_log));
    let waiter = tokio::task::spawn_blocking(move || wait4(pid));
    tokio::pin!(waiter);
    let mut stopped: Option<StopReason> = None;
    let deadline = spec.deadline;
    let status = loop {
        let until_deadline = async {
            match deadline {
                Some((at, _)) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            done = &mut waiter => break done.map_err(std::io::Error::other)??,
            () = until_deadline, if stopped.is_none() => {
                stopped = deadline.map(|(_, why)| why);
                kill_group(pid, nix::sys::signal::Signal::SIGTERM);
            }
            changed = spec.stop.changed(), if stopped.is_none() => {
                let why = if changed.is_ok() { *spec.stop.borrow() } else { None };
                if let Some(why) = why {
                    stopped = Some(why);
                    kill_group(pid, nix::sys::signal::Signal::SIGTERM);
                } else if changed.is_err() {
                    // The sender is gone: nothing will stop it but its end.
                    let status = (&mut waiter).await.map_err(std::io::Error::other)??;
                    break status;
                }
            }
        }
        if stopped.is_some() {
            if let Ok(done) = tokio::time::timeout(spec.kill_grace, &mut waiter).await {
                break done.map_err(std::io::Error::other)??;
            }
            kill_group(pid, nix::sys::signal::Signal::SIGKILL);
            break (&mut waiter).await.map_err(std::io::Error::other)??;
        }
    };
    let wall = started.elapsed();
    // Whatever it left behind in the job's directory (its group was
    // killed before the leader was reaped, so the group id was still its).
    sweep(&spec.sweep);
    // The log ends when the last writer is gone; one that escaped the
    // sweep is not waited for.
    let (kept, dropped, head) = join_logger(logger).await;
    let ended = match (stopped, status.ended) {
        (Some(why), _) => Ended::Stopped(why),
        (None, Exit::Code(71)) if spec.seatbelt && head.starts_with(b"sandbox-exec: ") => {
            Ended::NotStarted(
                String::from_utf8_lossy(&head)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
            )
        }
        (None, Exit::Code(code)) => Ended::Exited(code),
        (None, Exit::Signal(signal)) => Ended::Signaled(signal),
    };
    let mut ran = Ran {
        ended,
        wall,
        cpu: status.cpu,
        logged: kept,
        dropped,
    };
    if dropped > 0 {
        // The log says how much it lost.
        if let Ok(mut log) = std::fs::OpenOptions::new().append(true).open(spec.log) {
            let _ = writeln!(
                log,
                "\nreef executor: {dropped} bytes of output dropped past [run] max_log"
            );
            ran.logged = log.metadata().map_or(ran.logged, |m| m.len());
        }
    }
    Ok(ran)
}

fn not_started(message: String, started: Instant) -> Ran {
    Ran {
        ended: Ended::NotStarted(message),
        wall: started.elapsed(),
        cpu: Duration::ZERO,
        logged: 0,
        dropped: 0,
    }
}

/// Puts the child in a session of its own, so that its process group is
/// its pid and it has no controlling terminal.
#[allow(unsafe_code)]
fn new_session(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: `setsid` is async-signal-safe and touches no memory of the
    // parent's; it runs in the forked child before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

enum Exit {
    Code(i32),
    Signal(i32),
}

struct Status {
    ended: Exit,
    cpu: Duration,
}

/// Waits for `pid` to end, kills what is left of its process group while
/// the group id is still its (the leader is not yet reaped, so it cannot be
/// reused), then reaps it with its resource usage.
#[allow(unsafe_code)]
fn wait4(pid: i32) -> std::io::Result<Status> {
    loop {
        // SAFETY: zeroed siginfo is a valid value of a plain C struct.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: a live local of the right type.
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                libc::id_t::try_from(pid).unwrap_or(0),
                &raw mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if waited == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    kill_group(pid, nix::sys::signal::Signal::SIGKILL);
    loop {
        let mut status = 0;
        // SAFETY: zeroed rusage is a valid value of a plain C struct.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers are to live locals of the right types.
        let reaped = unsafe { libc::wait4(pid, &raw mut status, 0, &raw mut usage) };
        if reaped == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        let time = |t: libc::timeval| {
            Duration::from_secs(u64::try_from(t.tv_sec).unwrap_or(0))
                + Duration::from_micros(u64::try_from(t.tv_usec).unwrap_or(0))
        };
        let cpu = time(usage.ru_utime) + time(usage.ru_stime);
        let ended = if libc::WIFEXITED(status) {
            Exit::Code(libc::WEXITSTATUS(status))
        } else if libc::WIFSIGNALED(status) {
            Exit::Signal(libc::WTERMSIG(status))
        } else {
            continue;
        };
        return Ok(Status { ended, cpu });
    }
}

fn kill_group(pid: i32, signal: nix::sys::signal::Signal) {
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), signal);
}

/// Copies `reader` to `log` until every writer is gone, keeping at most
/// `max` bytes. Returns what was kept and dropped, and the first bytes.
fn copy_capped(
    mut reader: std::io::PipeReader,
    log: &mut std::fs::File,
    max: u64,
) -> (u64, u64, Vec<u8>) {
    let mut buffer = vec![0u8; 1 << 16];
    let (mut kept, mut dropped) = (0u64, 0u64);
    let mut head = Vec::new();
    loop {
        let n = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if head.len() < 256 {
            let take = (256 - head.len()).min(n);
            head.extend_from_slice(&buffer[..take]);
        }
        let room = usize::try_from(max.saturating_sub(kept)).unwrap_or(usize::MAX);
        let keep = room.min(n);
        if keep > 0 && log.write_all(&buffer[..keep]).is_ok() {
            kept += keep as u64;
        }
        dropped += (n - keep) as u64;
    }
    let _ = log.flush();
    (kept, dropped, head)
}

async fn join_logger(logger: std::thread::JoinHandle<(u64, u64, Vec<u8>)>) -> (u64, u64, Vec<u8>) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !logger.is_finished() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if logger.is_finished() {
        logger.join().unwrap_or_default()
    } else {
        tracing::warn!(
            "a check's output is still open after it ended: a process escaped the sweep"
        );
        (0, 0, Vec::new())
    }
}

/// Kills, with `SIGKILL`, every process of this user whose working
/// directory is `dir` or under it. On Linux a bubblewrap check's PID
/// namespace has already taken them; this covers `kind = "none"`, macOS,
/// and processes that left the check's group.
pub fn sweep(dir: &Path) {
    let dir = &crate::sandbox::real_path(dir);
    let me = std::process::id();
    for pid in processes() {
        if pid == me || pid <= 1 {
            continue;
        }
        if let Some(cwd) = cwd_of(pid)
            && cwd.starts_with(dir)
        {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap_or(0)),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn processes() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .map(|dir| {
            dir.filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn cwd_of(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn processes() -> Vec<u32> {
    // SAFETY: a null buffer asks for the count; the second call writes at
    // most `buffer.len()` pids into a buffer of that many.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    let mut buffer = vec![0 as libc::pid_t; count + 64];
    let bytes = i32::try_from(buffer.len() * size_of::<libc::pid_t>()).unwrap_or(0);
    // SAFETY: as above.
    let got = unsafe { libc::proc_listallpids(buffer.as_mut_ptr().cast(), bytes) };
    let got = usize::try_from(got).unwrap_or(0).min(buffer.len());
    buffer[..got]
        .iter()
        .filter_map(|pid| u32::try_from(*pid).ok())
        .collect()
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn cwd_of(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    // SAFETY: zeroed is a valid value of this plain C struct, and
    // proc_pidinfo writes at most its size.
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = i32::try_from(size_of::<libc::proc_vnodepathinfo>()).ok()?;
    // SAFETY: `info` is a live local of `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid).ok()?,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    let path = &info.pvi_cdir.vip_path;
    // SAFETY: c_char and u8 have the same layout.
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(path.as_ptr().cast(), path.len() * path[0].len()) };
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    (end > 0).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(&bytes[..end])))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn processes() -> Vec<u32> {
    Vec::new()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cwd_of(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec<'a>(
        dir: &'a Path,
        argv: &[&str],
        log: &'a Path,
        stop: watch::Receiver<Option<StopReason>>,
    ) -> Spec<'a> {
        let mut command = Command::new(argv[0]);
        command.args(&argv[1..]).current_dir(dir);
        Spec {
            command,
            program: "/bin/sh",
            path: "/usr/bin:/bin".as_ref(),
            log,
            max_log: 1 << 20,
            deadline: None,
            stop,
            kill_grace: Duration::from_millis(500),
            sweep: dir.to_owned(),
            seatbelt: false,
        }
    }

    #[tokio::test]
    async fn exit_codes_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let (_tx, rx) = watch::channel(None);
        let ran = run(spec(
            dir.path(),
            &["/bin/sh", "-c", "echo out; echo err >&2; exit 3"],
            &log,
            rx,
        ))
        .await
        .unwrap();
        assert_eq!(ran.ended, Ended::Exited(3));
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("out") && text.contains("err"), "{text}");
    }

    #[tokio::test]
    async fn a_missing_program_never_starts() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let (_tx, rx) = watch::channel(None);
        let mut s = spec(dir.path(), &["/bin/sh"], &log, rx);
        s.program = "no-such-program";
        let ran = run(s).await.unwrap();
        assert!(matches!(ran.ended, Ended::NotStarted(_)), "{:?}", ran.ended);
    }

    #[tokio::test]
    async fn the_deadline_stops_it_and_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let pidfile = dir.path().join("pid");
        let (_tx, rx) = watch::channel(None);
        // A child that ignores SIGTERM and leaves the group.
        let script = format!(
            "(trap '' TERM; exec perl -e 'setpgrp(0,0); $SIG{{TERM}}=\"IGNORE\"; sleep 60') & echo $! > {}; sleep 60",
            pidfile.display()
        );
        let mut s = spec(dir.path(), &["/bin/sh", "-c", &script], &log, rx);
        s.deadline = Some((
            tokio::time::Instant::now() + Duration::from_millis(500),
            StopReason::Timeout,
        ));
        let ran = run(s).await.unwrap();
        assert_eq!(ran.ended, Ended::Stopped(StopReason::Timeout));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
        assert!(!alive, "the escaped child {pid} is still running");
    }

    #[tokio::test]
    async fn a_stop_signal_stops_it() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let (tx, rx) = watch::channel(None);
        let s = spec(dir.path(), &["/bin/sh", "-c", "sleep 60"], &log, rx);
        let stopper = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            tx.send(Some(StopReason::Cancelled)).unwrap();
        };
        let (ran, ()) = tokio::join!(run(s), stopper);
        assert_eq!(ran.unwrap().ended, Ended::Stopped(StopReason::Cancelled));
    }

    #[tokio::test]
    async fn logs_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let (_tx, rx) = watch::channel(None);
        let mut s = spec(
            dir.path(),
            &["/bin/sh", "-c", "head -c 100000 /dev/zero"],
            &log,
            rx,
        );
        s.max_log = 1000;
        let ran = run(s).await.unwrap();
        assert_eq!(ran.dropped, 99_000);
        let text = std::fs::read(&log).unwrap();
        assert!(String::from_utf8_lossy(&text).contains("99000 bytes of output dropped"));
    }

    #[tokio::test]
    async fn cpu_time_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let (_tx, rx) = watch::channel(None);
        let ran = run(spec(
            dir.path(),
            &[
                "/bin/sh",
                "-c",
                "i=0; while [ $i -lt 200000 ]; do i=$((i+1)); done",
            ],
            &log,
            rx,
        ))
        .await
        .unwrap();
        assert_eq!(ran.ended, Ended::Exited(0));
        assert!(ran.cpu > Duration::ZERO);
    }
}
