//! The holder of a provider's workspace (R2.26): the process whose life a
//! follower's lease renewals follow, and the record a follower keeps
//! under `<state>/holders/` while it runs.
//!
//! A holder is a pid and that process's start time, as the system records
//! it (seconds and microseconds since the epoch on macOS, clock ticks since
//! boot on Linux), so a reused pid is never taken for the holder. The start
//! time is compared as recorded and never converted.

use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use reef_api::v1;
use reef_error::Advice;
use serde::{Deserialize, Serialize};

use super::{CliError, Global};

/// A `hold_while` naming no running process, or a `parent` walk that
/// found none.
pub(crate) const HOLDER_NOT_RUNNING: &str = "HOLDER_NOT_RUNNING";

/// The shells `parent` walks past, by command name.
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "ksh", "mksh", "fish", "csh", "tcsh", "nu", "pwsh",
];
/// How many ancestors `parent` looks at.
const PARENT_LEVELS: usize = 8;

/// What `hold_while` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HoldWhile {
    /// This process.
    Pid(i32),
    /// The nearest ancestor of this process that is not a shell.
    Parent,
}

impl HoldWhile {
    /// `parent`, or a pid in decimal; `None` for anything else.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        if text == "parent" {
            return Some(Self::Parent);
        }
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        text.parse().ok().map(Self::Pid)
    }
}

/// A running process, by pid and start time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Holder {
    pub(crate) pid: i32,
    pub(crate) started_at: u64,
}

/// What the system says about one process.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcInfo {
    parent: i32,
    /// The command name, a leading `-` (a login shell) removed.
    name: String,
    started_at: u64,
}

/// The process `hold_while` names, which must be running and not pid 1.
pub(crate) fn resolve(hold_while: HoldWhile) -> Result<Holder, CliError> {
    match hold_while {
        HoldWhile::Pid(pid) => {
            let info = info(pid)
                .filter(|_| pid > 1)
                .ok_or_else(|| not_running(&format!("process {pid} is not running")))?;
            Ok(Holder {
                pid,
                started_at: info.started_at,
            })
        }
        HoldWhile::Parent => parent_of(i32::try_from(std::process::id()).unwrap_or(0)),
    }
}

/// The nearest ancestor of `pid` whose command is not a shell.
fn parent_of(pid: i32) -> Result<Holder, CliError> {
    let mut current = info(pid).map(|info| info.parent);
    for _ in 0..PARENT_LEVELS {
        let Some(pid) = current.filter(|pid| *pid > 1) else {
            break;
        };
        let Some(info) = info(pid) else {
            break;
        };
        if !SHELLS.contains(&info.name.as_str()) {
            return Ok(Holder {
                pid,
                started_at: info.started_at,
            });
        }
        current = Some(info.parent);
    }
    Err(not_running(&format!(
        "no ancestor of this process within {PARENT_LEVELS} levels is a running program other than a shell"
    )))
}

fn not_running(message: &str) -> CliError {
    CliError::Input {
        reason: HOLDER_NOT_RUNNING,
        message: message.to_owned(),
        field: "hold_while",
        advice: Advice::hint(
            "Name a running process's pid, or `parent` from a program's hook; the workspace is held while it runs.",
        ),
    }
}

/// Whether `holder` is still the process it was: running, not a zombie,
/// with the same start time.
pub(crate) fn alive(holder: &Holder) -> bool {
    info(holder.pid).is_some_and(|info| info.started_at == holder.started_at)
}

#[cfg(target_os = "macos")]
fn info(pid: i32) -> Option<ProcInfo> {
    let mut bsd = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = i32::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    #[allow(
        unsafe_code,
        reason = "proc_pidinfo writes at most `size` bytes into the buffer it is given"
    )]
    let (written, bsd) = unsafe {
        let written = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            bsd.as_mut_ptr().cast(),
            size,
        );
        (written, bsd.assume_init())
    };
    if written != size || bsd.pbi_status == libc::SZOMB {
        return None;
    }
    let name: Vec<u8> = bsd
        .pbi_comm
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| byte.to_ne_bytes()[0])
        .collect();
    Some(ProcInfo {
        parent: i32::try_from(bsd.pbi_ppid).ok()?,
        name: login_shell(&String::from_utf8_lossy(&name)),
        started_at: bsd
            .pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(bsd.pbi_start_tvusec),
    })
}

#[cfg(target_os = "linux")]
fn info(pid: i32) -> Option<ProcInfo> {
    parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// `/proc/<pid>/stat`: `pid (comm) state ppid …`, with the start time in
/// field 22. `comm` may hold spaces and parentheses, so it ends at the
/// last `)`.
#[cfg(any(target_os = "linux", test))]
fn parse_stat(stat: &str) -> Option<ProcInfo> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?;
    let rest: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    // rest[0] is field 3 (state), rest[1] field 4 (ppid), rest[19] field 22.
    if matches!(rest.first(), Some(&("Z" | "X" | "x"))) {
        return None;
    }
    Some(ProcInfo {
        parent: rest.get(1)?.parse().ok()?,
        name: login_shell(name),
        started_at: rest.get(19)?.parse().ok()?,
    })
}

fn login_shell(name: &str) -> String {
    name.strip_prefix('-').unwrap_or(name).to_owned()
}

/// What a follower records while it holds a workspace's lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) workspace_id: String,
    pub(crate) lease_id: String,
    pub(crate) follower: Holder,
    pub(crate) holder: Option<Holder>,
    /// When the follower started, RFC 3339.
    pub(crate) since: String,
}

/// `<state>/holders`.
fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("holders")
}

/// One file per follower, `<workspace id>.<follower pid>.json`, so a
/// follower only ever writes and removes its own: a successor's record is
/// never at risk from a predecessor that is slow to exit.
fn path(state_dir: &Path, workspace_id: &str, follower: &Holder) -> PathBuf {
    dir(state_dir).join(format!("{workspace_id}.{}.json", follower.pid))
}

/// Writes `record` through a temporary file renamed into place, in a
/// directory only this user can read.
pub(crate) fn write(state_dir: &Path, record: &Record) -> io::Result<()> {
    let dir = dir(state_dir);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let final_path = path(state_dir, &record.workspace_id, &record.follower);
    let temp = dir.join(format!(
        ".{}.{}.tmp",
        record.workspace_id, record.follower.pid
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec(record).map_err(io::Error::other)?)?;
    file.sync_all()?;
    fs::rename(&temp, &final_path)
}

/// Every record for `workspace_id`, current or not.
pub(crate) fn read(state_dir: &Path, workspace_id: &str) -> Vec<Record> {
    let prefix = format!("{workspace_id}.");
    let Ok(entries) = fs::read_dir(dir(state_dir)) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(&prefix)
                && Path::new(name.as_ref())
                    .extension()
                    .is_some_and(|extension| extension == "json")
        })
        .filter_map(|entry| serde_json::from_slice(&fs::read(entry.path()).ok()?).ok())
        .filter(|record: &Record| record.workspace_id == workspace_id)
        .collect()
}

/// Removes `follower`'s record for `workspace_id`.
pub(crate) fn remove(state_dir: &Path, workspace_id: &str, follower: &Holder) {
    let path = path(state_dir, workspace_id, follower);
    let ours = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
        .is_some_and(|record| record.follower == *follower);
    if ours {
        let _ = fs::remove_file(path);
    }
}

/// The newest current record for `ws`: its follower is running and its
/// lease is the workspace's. Stale ones are removed, quietly.
pub(crate) fn current(state_dir: &Path, ws: &v1::Workspace) -> Option<Record> {
    let lease = ws.lease.as_ref().map_or("", |lease| lease.id.as_str());
    let mut live = Vec::new();
    for record in read(state_dir, &ws.id) {
        if alive(&record.follower) && record.lease_id == lease {
            live.push(record);
        } else {
            remove(state_dir, &ws.id, &record.follower);
        }
    }
    live.into_iter().max_by(|a, b| a.since.cmp(&b.since))
}

/// Asks the follower in `record` to stop, and waits up to two seconds for
/// it to go.
pub(crate) fn stop(record: &Record) {
    if record.follower.pid <= 1 || !alive(&record.follower) {
        return;
    }
    let pid = nix::unistd::Pid::from_raw(record.follower.pid);
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while alive(&record.follower) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// `<state>/logs/follower-<id>.log`, the follower's stderr.
pub(crate) fn log_path(state_dir: &Path, workspace_id: &str) -> PathBuf {
    state_dir
        .join("logs")
        .join(format!("follower-{workspace_id}.log"))
}

/// A follower that could not be started: kind `io`, exit 8.
fn spawn_failed(op: &str, path: &Path, source: &io::Error) -> CliError {
    CliError::Refused {
        kind: reef_error::ErrorKind::Io,
        message: format!(
            "could not start the lease follower: {op} {}: {source}",
            path.display()
        ),
        advice: Some(Advice::hint(
            "Check that the state directory's logs/ is writable; nothing was left behind.",
        )),
    }
}

/// Starts `reef ws renew --follow` for `lease` of `workspace_id`, detached,
/// with the path flags this command was given, and returns its pid. It
/// does not wait: a hook must return. The follower keeps no descriptor of
/// the caller's but its log, so it never holds a pipe of the program that
/// ran the hook open.
pub(crate) fn spawn_follower(
    global: &Global,
    agent: Option<&str>,
    workspace_id: &str,
    lease_id: &str,
    holder: &Holder,
) -> Result<u32, CliError> {
    let state_dir = global.state_dir()?;
    let log = log_path(&state_dir, workspace_id);
    if let Some(logs) = log.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(logs)
            .map_err(|error| spawn_failed("create", logs, &error))?;
    }
    let stderr = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&log)
        .map_err(|error| spawn_failed("open", &log, &error))?;
    let exe =
        std::env::current_exe().map_err(|error| spawn_failed("find", Path::new("reef"), &error))?;
    let mut command = Command::new(&exe);
    super::autostart::path_flags(&mut command, global)?;
    if let Some(agent) = agent {
        command.arg("--agent").arg(agent);
    }
    // A command told not to start a daemon starts none through its
    // follower either.
    if global.no_autostart {
        command.arg("--no-autostart");
    }
    command
        .args(["ws", "renew", "--follow", "--while-pid"])
        .arg(holder.pid.to_string())
        .arg("--lease-id")
        .arg(lease_id)
        .arg(workspace_id)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[allow(
        unsafe_code,
        reason = "the closure only calls close(2), which is async-signal-safe"
    )]
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, || {
            close_inherited();
            Ok(())
        });
    }
    let child = command
        .spawn()
        .map_err(|error| spawn_failed("start", &exe, &error))?;
    Ok(child.id())
}

/// Closes the descriptors above stderr that would survive exec, between
/// fork and exec: those the program running the hook passed down without
/// close-on-exec. Close-on-exec ones, such as the pipe `spawn` reports an
/// exec failure through, are left to exec.
fn close_inherited() {
    // `sysconf` is not on the async-signal-safe list; a fixed bound is.
    const MAX_FD: i32 = 4096;
    for fd in 3..MAX_FD {
        #[allow(
            unsafe_code,
            reason = "fcntl(F_GETFD) and close(2) on a descriptor number, open or not"
        )]
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                libc::close(fd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn hold_while_parses() {
        assert_eq!(HoldWhile::parse("parent"), Some(HoldWhile::Parent));
        assert_eq!(HoldWhile::parse("42"), Some(HoldWhile::Pid(42)));
        for bad in ["", "12x", "-3", "Parent", " 4", "99999999999"] {
            assert_eq!(HoldWhile::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn stat_is_parsed_past_odd_names() {
        let fields = (5..=22)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let info = parse_stat(&format!("123 (we (ird) name) S 77 {fields} 23 24")).unwrap();
        assert_eq!(info.parent, 77);
        assert_eq!(info.name, "we (ird) name");
        assert_eq!(info.started_at, 22);
        let login = parse_stat(&format!("5 (-zsh) S 1 {fields}")).unwrap();
        assert_eq!(login.name, "zsh");
        assert_eq!(parse_stat(&format!("5 (x) Z 1 {fields}")), None);
    }

    /// R2.26 test 4: a child is alive with its start time, not with
    /// another, nor once it is killed and reaped.
    #[test]
    fn alive_checks_start_time() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let holder = resolve(HoldWhile::Pid(pid)).unwrap();
        assert!(alive(&holder));
        assert!(!alive(&Holder {
            started_at: holder.started_at + 1,
            ..holder
        }));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!alive(&holder));
        assert!(resolve(HoldWhile::Pid(pid)).is_err());
        assert!(resolve(HoldWhile::Pid(1)).is_err());
    }

    /// R2.26 test 3: `parent` walks past shells to the first program.
    #[test]
    fn parent_skips_shells() {
        /// Kills a process group when dropped, so a failed test leaks
        /// nothing.
        struct Group(std::process::Child);
        impl Drop for Group {
            fn drop(&mut self) {
                let group = nix::unistd::Pid::from_raw(i32::try_from(self.0.id()).unwrap());
                let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
                let _ = self.0.wait();
            }
        }

        let me = resolve(HoldWhile::Pid(i32::try_from(std::process::id()).unwrap())).unwrap();
        // `; true` keeps each shell from exec'ing its last command, so the
        // chain is this test, sh, bash, sleep.
        let mut command = Command::new("sh");
        command.args(["-c", "bash -c 'sleep 30; true'; true"]);
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let group = Group(command.spawn().unwrap());
        let sh = i32::try_from(group.0.id()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let sleep = loop {
            if let Some(pid) = descendants_named(sh, "sleep") {
                break pid;
            }
            assert!(std::time::Instant::now() < deadline, "no sleep under {sh}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        // From `sleep`, a program, the nearest non-shell ancestor past
        // bash and sh is this test.
        assert_eq!(parent_of(sleep).unwrap(), me);
        drop(group);
    }

    /// A process named `name` among `root`'s children and grandchildren.
    fn descendants_named(root: i32, name: &str) -> Option<i32> {
        let out = Command::new("ps")
            .args(["-axo", "pid=,ppid=,comm="])
            .output()
            .ok()?;
        let rows: Vec<(i32, i32, String)> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let child = parts.next()?.parse().ok()?;
                let parent = parts.next()?.parse().ok()?;
                let comm: String = parts.collect::<Vec<_>>().join(" ");
                Some((child, parent, comm))
            })
            .collect();
        let children = |of: i32| rows.iter().filter(move |row| row.1 == of);
        children(root)
            .flat_map(|child| std::iter::once(child).chain(children(child.0)))
            .find(|row| row.2.rsplit('/').next() == Some(name))
            .map(|row| row.0)
    }

    /// R2.26 test 5: a record round-trips, and a stale one is removed.
    #[test]
    fn holder_record_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let me = resolve(HoldWhile::Pid(i32::try_from(std::process::id()).unwrap())).unwrap();
        let record = Record {
            workspace_id: "w1".to_owned(),
            lease_id: "l1".to_owned(),
            follower: me,
            holder: Some(me),
            since: "2026-11-01T00:00:00.000Z".to_owned(),
        };
        write(tmp.path(), &record).unwrap();
        assert_eq!(read(tmp.path(), "w1"), vec![record.clone()]);
        let mode = fs::metadata(tmp.path().join("holders"))
            .unwrap()
            .permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o700
        );

        let ws = |lease: &str| v1::Workspace {
            id: "w1".to_owned(),
            lease: Some(v1::Lease {
                id: lease.to_owned(),
                ..v1::Lease::default()
            }),
            ..v1::Workspace::default()
        };
        assert_eq!(current(tmp.path(), &ws("l1")), Some(record.clone()));
        // Another lease: stale, and removed.
        assert_eq!(current(tmp.path(), &ws("l2")), None);
        assert_eq!(read(tmp.path(), "w1"), vec![]);

        // A dead follower: stale.
        let gone = Record {
            follower: Holder {
                started_at: me.started_at + 1,
                ..me
            },
            ..record.clone()
        };
        write(tmp.path(), &gone).unwrap();
        assert_eq!(current(tmp.path(), &ws("l1")), None);
        assert_eq!(read(tmp.path(), "w1"), vec![]);

        // Two followers' records sit side by side, and the current one is
        // the newest live one; one never removes the other's.
        let other = Holder {
            pid: me.pid + 1,
            started_at: me.started_at,
        };
        let older = Record {
            follower: other,
            since: "2026-10-31T00:00:00.000Z".to_owned(),
            ..record.clone()
        };
        write(tmp.path(), &older).unwrap();
        write(tmp.path(), &record).unwrap();
        assert_eq!(read(tmp.path(), "w1").len(), 2);
        remove(tmp.path(), "w1", &other);
        assert_eq!(read(tmp.path(), "w1"), vec![record.clone()]);
        write(tmp.path(), &older).unwrap();
        remove(tmp.path(), "w1", &me);
        assert_eq!(read(tmp.path(), "w1"), vec![older]);
    }
}
