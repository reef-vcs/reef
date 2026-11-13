//! What the machine is doing, once a second: free space, load, CPU, memory
//! pressure, swap, and each running ticket's memory; and the canary, which
//! measures how responsive the machine stays while agents build.
//!
//! The readers are plain commands and `/proc` files, parsed by functions
//! tested on fixtures: `ps` (process tree RSS, CPU), `vm_stat` and `sysctl`
//! on macOS, `/proc/pressure/memory`, `/proc/meminfo` and `/proc/stat` on
//! Linux. Sampling costs a few process spawns a second, well under 1% of a
//! core.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::disk;

/// How often the sampler reads.
pub const INTERVAL: Duration = Duration::from_secs(1);
/// macOS's important-usage capacity is read every this many samples (a
/// read costs about 60 ms).
const IMPORTANT_EVERY: u64 = 5;

/// One reading.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// Milliseconds since the scenario started.
    pub t_ms: u64,
    /// `statvfs` free bytes on each watched volume, in [`Sampler`]'s order.
    pub free_bytes: Vec<u64>,
    /// macOS: the first volume's free space counting purgeable space, every
    /// few samples.
    pub important_free_bytes: Option<u64>,
    /// The 1-minute load average.
    pub load1: f64,
    /// The share of all cores busy, 0 to 1.
    pub cpu_busy: Option<f64>,
    /// macOS memory pressure: 1 normal, 2 warn, 4 critical.
    pub pressure_level: Option<u32>,
    /// Linux PSI `memory some avg10`, percent.
    pub psi_some_avg10: Option<f64>,
    /// Linux PSI `memory full avg10`, percent.
    pub psi_full_avg10: Option<f64>,
    /// macOS: bytes swapped out since boot (`vm_stat` Swapouts × page size).
    pub swapout_bytes: Option<u64>,
    /// Swap in use, bytes.
    pub swap_used_bytes: Option<u64>,
    /// macOS: the compressor's resident bytes.
    pub compressor_bytes: Option<u64>,
    /// Each running ticket's process tree RSS, bytes.
    pub ticket_rss: BTreeMap<String, u64>,
}

/// A named span of the scenario.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase {
    /// `solo`, `solo-1`, `five`, `idle`, `release`, `gc`.
    pub name: String,
    /// Milliseconds since the scenario started.
    pub start_ms: u64,
    /// Milliseconds since the scenario started.
    pub end_ms: u64,
}

/// The processes a sampler follows: a label per root pid (a `ws run`).
#[derive(Clone, Default)]
pub struct Tracked(Arc<Mutex<HashMap<String, u32>>>);

impl Tracked {
    /// Follows `pid`'s process tree as `label` until [`Tracked::remove`].
    pub fn add(&self, label: &str, pid: u32) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(label.to_owned(), pid);
    }

    /// Stops following `label`.
    pub fn remove(&self, label: &str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(label);
    }

    fn snapshot(&self) -> HashMap<String, u32> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The background sampler.
pub struct Sampler {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Vec<Sample>>>,
}

impl Sampler {
    /// Starts sampling `volumes` (paths on each volume to watch) and the
    /// processes in `tracked`, with `t_ms` counted from `epoch`.
    pub fn start(volumes: Vec<PathBuf>, tracked: Tracked, epoch: Instant) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut samples = Vec::new();
            let mut cpu = CpuReader::default();
            let cores = cores();
            let mut n = 0_u64;
            while !flag.load(Ordering::Relaxed) {
                let tick = Instant::now();
                let sample = read(
                    &volumes,
                    &tracked,
                    epoch,
                    &mut cpu,
                    cores,
                    n,
                    samples.last(),
                );
                samples.push(sample);
                n += 1;
                let spent = tick.elapsed();
                // Sleep in short steps so a stop is quick.
                let mut left = INTERVAL.saturating_sub(spent);
                while !left.is_zero() && !flag.load(Ordering::Relaxed) {
                    let nap = left.min(Duration::from_millis(100));
                    std::thread::sleep(nap);
                    left = left.saturating_sub(nap);
                }
            }
            samples
        });
        Sampler {
            stop,
            handle: Some(handle),
        }
    }

    /// Stops and returns every sample.
    pub fn finish(mut self) -> Vec<Sample> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments, reason = "the sampler's state, passed in")]
fn read(
    volumes: &[PathBuf],
    tracked: &Tracked,
    epoch: Instant,
    cpu: &mut CpuReader,
    cores: u32,
    n: u64,
    last: Option<&Sample>,
) -> Sample {
    let mut sample = Sample {
        t_ms: millis(epoch.elapsed()),
        // A failed `statvfs` keeps the last reading, never a 0 that would
        // look like a full disk.
        free_bytes: volumes
            .iter()
            .enumerate()
            .map(|(i, v)| {
                disk::free_bytes(v)
                    .ok()
                    .or_else(|| last.and_then(|l| l.free_bytes.get(i).copied()))
                    .unwrap_or(0)
            })
            .collect(),
        load1: load1(),
        ..Sample::default()
    };
    if n.is_multiple_of(IMPORTANT_EVERY) {
        sample.important_free_bytes = volumes.first().and_then(|v| disk::important_free_bytes(v));
    }
    let procs = command("ps", &["-A", "-o", "pid=,ppid=,rss=,%cpu="])
        .map(|out| parse_ps(&out))
        .unwrap_or_default();
    for (label, pid) in tracked.snapshot() {
        sample.ticket_rss.insert(label, tree_rss_bytes(&procs, pid));
    }
    sample.cpu_busy = cpu.read(&procs, cores);
    if cfg!(target_os = "macos") {
        sample.pressure_level = command("sysctl", &["-n", "kern.memorystatus_vm_pressure_level"])
            .and_then(|out| out.trim().parse().ok());
        if let Some(vm) = command("vm_stat", &[]).map(|out| parse_vm_stat(&out)) {
            sample.swapout_bytes = vm.swapout_bytes;
            sample.compressor_bytes = vm.compressor_bytes;
        }
        sample.swap_used_bytes =
            command("sysctl", &["-n", "vm.swapusage"]).and_then(|out| parse_swapusage(&out));
    } else {
        if let Ok(text) = std::fs::read_to_string("/proc/pressure/memory") {
            let psi = parse_psi(&text);
            sample.psi_some_avg10 = psi.some_avg10;
            sample.psi_full_avg10 = psi.full_avg10;
        }
        if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
            sample.swap_used_bytes = parse_meminfo_swap(&text);
        }
    }
    sample
}

fn command(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Milliseconds, saturating.
pub fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The 1-minute load average.
pub fn load1() -> f64 {
    let mut loads = [0.0_f64; 3];
    #[allow(
        unsafe_code,
        reason = "FFI; the buffer holds the three values asked for"
    )]
    // SAFETY: `loads` has room for the 3 values requested.
    let n = unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) };
    if n >= 1 { loads[0] } else { 0.0 }
}

/// Logical cores.
pub fn cores() -> u32 {
    std::thread::available_parallelism()
        .ok()
        .and_then(|n| u32::try_from(n.get()).ok())
        .unwrap_or(1)
}

/// One `ps -A -o pid=,ppid=,rss=,%cpu=` row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Proc {
    /// Process id.
    pub pid: u32,
    /// Parent's.
    pub ppid: u32,
    /// Resident set, KiB.
    pub rss_kib: u64,
    /// CPU percent of one core.
    pub cpu_percent: f64,
}

/// Parses `ps -A -o pid=,ppid=,rss=,%cpu=`.
pub fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(Proc {
                pid: fields.next()?.parse().ok()?,
                ppid: fields.next()?.parse().ok()?,
                rss_kib: fields.next()?.parse().ok()?,
                cpu_percent: fields.next()?.replace(',', ".").parse().ok()?,
            })
        })
        .collect()
}

/// The summed RSS of `root` and every descendant, bytes.
pub fn tree_rss_bytes(procs: &[Proc], root: u32) -> u64 {
    let mut children: HashMap<u32, Vec<&Proc>> = HashMap::new();
    for proc in procs {
        children.entry(proc.ppid).or_default().push(proc);
    }
    let mut total = 0;
    let mut stack: Vec<u32> = procs
        .iter()
        .filter(|p| p.pid == root)
        .map(|p| {
            total += p.rss_kib;
            p.pid
        })
        .collect();
    while let Some(pid) = stack.pop() {
        for child in children.get(&pid).into_iter().flatten() {
            if child.pid != pid {
                total += child.rss_kib;
                stack.push(child.pid);
            }
        }
    }
    total * 1024
}

/// `vm_stat`'s figures the gates read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VmStat {
    /// Swapouts × page size.
    pub swapout_bytes: Option<u64>,
    /// "Pages occupied by compressor" × page size.
    pub compressor_bytes: Option<u64>,
}

/// Parses macOS's `vm_stat`.
pub fn parse_vm_stat(text: &str) -> VmStat {
    let page = text
        .lines()
        .next()
        .and_then(|header| header.split("page size of ").nth(1))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(4096);
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|value| value.trim().trim_end_matches('.').parse::<u64>().ok())
            .map(|pages| pages * page)
    };
    VmStat {
        swapout_bytes: field("Swapouts"),
        compressor_bytes: field("Pages occupied by compressor"),
    }
}

/// Parses `sysctl -n vm.swapusage`: `total = 2048.00M  used = 1024.50M
/// free = 1023.50M  (encrypted)`.
pub fn parse_swapusage(text: &str) -> Option<u64> {
    let used = text.split("used = ").nth(1)?.split_whitespace().next()?;
    let (number, unit) = used.split_at(used.len().checked_sub(1)?);
    let value: f64 = number.parse().ok()?;
    let scale = match unit {
        "K" => 1024.0,
        "M" => 1024.0 * 1024.0,
        "G" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a non-negative size well under u64::MAX"
    )]
    Some((value * scale) as u64)
}

/// PSI's `avg10` figures.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Psi {
    /// `some avg10`.
    pub some_avg10: Option<f64>,
    /// `full avg10`.
    pub full_avg10: Option<f64>,
}

/// Parses `/proc/pressure/memory`.
pub fn parse_psi(text: &str) -> Psi {
    let avg10 = |kind: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(kind)?.strip_prefix(' '))
            .and_then(|rest| {
                rest.split_whitespace()
                    .find_map(|field| field.strip_prefix("avg10="))
            })
            .and_then(|value| value.parse().ok())
    };
    Psi {
        some_avg10: avg10("some"),
        full_avg10: avg10("full"),
    }
}

/// Swap in use from `/proc/meminfo`, bytes.
pub fn parse_meminfo_swap(text: &str) -> Option<u64> {
    let kib = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
    };
    Some(kib("SwapTotal")?.saturating_sub(kib("SwapFree")?) * 1024)
}

/// `/proc/stat`'s first line as (busy, total) jiffies.
pub fn parse_proc_stat(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    let total: u64 = values.iter().take(8).sum();
    // idle and iowait are the 4th and 5th.
    let idle = values.get(3)? + values.get(4).copied().unwrap_or(0);
    Some((total.saturating_sub(idle), total))
}

/// Reads the CPU's busy share: from `/proc/stat` deltas on Linux, from
/// `ps`'s summed `%cpu` elsewhere.
#[derive(Default)]
struct CpuReader {
    last: Option<(u64, u64)>,
}

impl CpuReader {
    #[allow(clippy::cast_precision_loss, reason = "a ratio")]
    fn read(&mut self, procs: &[Proc], cores: u32) -> Option<f64> {
        if let Ok(text) = std::fs::read_to_string("/proc/stat") {
            let now = parse_proc_stat(&text)?;
            let share = self.last.and_then(|(busy, total)| {
                let total = now.1.checked_sub(total).filter(|t| *t > 0)?;
                Some(now.0.saturating_sub(busy) as f64 / total as f64)
            });
            self.last = Some(now);
            return share;
        }
        let percent: f64 = procs.iter().map(|p| p.cpu_percent).sum();
        Some((percent / 100.0 / f64::from(cores.max(1))).min(1.0))
    }
}

/// The canary: how long `reef ws list` takes, every 5 seconds, and how late
/// a 10 ms sleep wakes, continuously.
pub struct Canary {
    stop: Arc<AtomicBool>,
    list: Option<JoinHandle<ListReadings>>,
    sleep: Option<JoinHandle<Vec<(u64, u32)>>>,
}

/// What the canary measured: (`t_ms`, value) pairs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CanaryReadings {
    /// `reef ws list -o json` latency, ms.
    pub list_ms: Vec<(u64, u64)>,
    /// When a `ws list` failed or ran past [`LIST_TIMEOUT`] (`t_ms`).
    pub list_failures: Vec<u64>,
    /// A 10 ms sleep's lateness, µs.
    pub lateness_us: Vec<(u64, u32)>,
}

/// Successful (`t_ms`, ms) pairs and failures' `t_ms`.
type ListReadings = (Vec<(u64, u64)>, Vec<u64>);

/// A `ws list` running this long is killed and counted as failed.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the canary lists workspaces.
const LIST_EVERY: Duration = Duration::from_secs(5);
/// The canary's sleep.
const SLEEP: Duration = Duration::from_millis(10);

impl Canary {
    /// Starts both loops; `list` builds the `reef ws list` command.
    pub fn start(list: impl Fn() -> Command + Send + 'static, epoch: Instant) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let list_handle = std::thread::spawn(move || {
            let (mut out, mut failed) = (Vec::new(), Vec::new());
            while !flag.load(Ordering::Relaxed) {
                let started = Instant::now();
                let ok = list()
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .is_ok_and(|mut child| {
                        loop {
                            match child.try_wait() {
                                Ok(Some(status)) => break status.success(),
                                Ok(None) if started.elapsed() < LIST_TIMEOUT => {
                                    std::thread::sleep(Duration::from_millis(20));
                                }
                                _ => {
                                    let _ = child.kill();
                                    let _ = child.wait();
                                    break false;
                                }
                            }
                        }
                    });
                if ok {
                    out.push((millis(epoch.elapsed()), millis(started.elapsed())));
                } else {
                    failed.push(millis(epoch.elapsed()));
                }
                let mut left = LIST_EVERY.saturating_sub(started.elapsed());
                while !left.is_zero() && !flag.load(Ordering::Relaxed) {
                    let nap = left.min(Duration::from_millis(100));
                    std::thread::sleep(nap);
                    left = left.saturating_sub(nap);
                }
            }
            (out, failed)
        });
        let flag = Arc::clone(&stop);
        let sleep_handle = std::thread::spawn(move || {
            let mut out = Vec::new();
            while !flag.load(Ordering::Relaxed) {
                let started = Instant::now();
                std::thread::sleep(SLEEP);
                let late = started.elapsed().saturating_sub(SLEEP);
                out.push((
                    millis(epoch.elapsed()),
                    u32::try_from(late.as_micros()).unwrap_or(u32::MAX),
                ));
            }
            out
        });
        Canary {
            stop,
            list: Some(list_handle),
            sleep: Some(sleep_handle),
        }
    }

    /// Stops both loops and returns what they measured.
    pub fn finish(mut self) -> CanaryReadings {
        self.stop.store(true, Ordering::Relaxed);
        let (list_ms, list_failures) = self
            .list
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        CanaryReadings {
            list_ms,
            list_failures,
            lateness_us: self
                .sleep
                .take()
                .and_then(|h| h.join().ok())
                .unwrap_or_default(),
        }
    }
}

impl Drop for Canary {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// How often the build limit is read.
pub const BUILD_EVERY: Duration = Duration::from_secs(5);

/// One reading of the daemon's build limit (#271), from `reef daemon
/// status -o json`'s `build`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildSample {
    /// Milliseconds since the scenario started.
    pub t_ms: u64,
    /// Tokens in the jobserver's FIFO.
    pub available: u32,
    /// The pool when no build runs.
    pub tokens: u32,
    /// Workspaces whose leases are heartbeating.
    pub builders: u32,
    /// What each tool without a jobserver gets.
    pub share: u32,
}

/// Reads the build limit every [`BUILD_EVERY`] until finished.
pub struct BuildSampler {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Vec<BuildSample>>>,
}

impl BuildSampler {
    /// Starts reading; `status` builds the `reef daemon status -o json`
    /// command. A daemon without a build limit gives no readings.
    pub fn start(status: impl Fn() -> Command + Send + 'static, epoch: Instant) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut out = Vec::new();
            while !flag.load(Ordering::Relaxed) {
                let started = Instant::now();
                if let Ok(output) = status().stdin(Stdio::null()).stderr(Stdio::null()).output()
                    && let Some(sample) = parse_build(&output.stdout, millis(epoch.elapsed()))
                {
                    out.push(sample);
                }
                let mut left = BUILD_EVERY.saturating_sub(started.elapsed());
                while !left.is_zero() && !flag.load(Ordering::Relaxed) {
                    let nap = left.min(Duration::from_millis(100));
                    std::thread::sleep(nap);
                    left = left.saturating_sub(nap);
                }
            }
            out
        });
        BuildSampler {
            stop,
            handle: Some(handle),
        }
    }

    /// Stops and returns the readings.
    pub fn finish(mut self) -> Vec<BuildSample> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for BuildSampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The `build` object of `daemon status -o json`'s output, at `t_ms`.
pub fn parse_build(stdout: &[u8], t_ms: u64) -> Option<BuildSample> {
    let value: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let build = value.get("build")?;
    let number = |key: &str| {
        build
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0)
    };
    Some(BuildSample {
        t_ms,
        available: number("available"),
        tokens: number("tokens"),
        builders: number("builders"),
        share: number("share"),
    })
}

/// The `p`th percentile (0 to 100) of `values`, nearest rank; `None` if
/// empty.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "an index into a short vector"
)]
pub fn percentile(mut values: Vec<f64>, p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * values.len() as f64).ceil() as usize;
    Some(values[rank.clamp(1, values.len()) - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_disk_reads_the_tightest_volume() {
        let json =
            br#"{"version":"0.4.2","disk":{"level":"warn","reef_bytes":7,"workspaces_bytes":5,
            "volumes":[{"free_bytes":100,"purgeable_free_bytes":null,"floor_bytes":20},
                       {"free_bytes":30,"purgeable_free_bytes":45,"floor_bytes":20}]}}"#;
        let sample = daemon_disk(json, 9).unwrap();
        assert_eq!(sample.level, "warn");
        assert_eq!((sample.free_bytes, sample.floor_bytes), (45, 20));
        assert_eq!(
            (sample.reef_bytes, sample.workspaces_bytes, sample.t_ms),
            (7, 5, 9)
        );
        assert!(daemon_disk(br#"{"version":"0.4.1"}"#, 0).is_none());
    }

    const VM_STAT: &str = "\
Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                                    20257.
Pages occupied by compressor:                 325715.
Swapins:                                           0.
Swapouts:                                       1000.
";

    #[test]
    fn parses_vm_stat() {
        assert_eq!(
            parse_vm_stat(VM_STAT),
            VmStat {
                swapout_bytes: Some(1000 * 16384),
                compressor_bytes: Some(325_715 * 16384),
            }
        );
    }

    #[test]
    fn parses_swapusage() {
        assert_eq!(
            parse_swapusage("total = 2048.00M  used = 1.50M  free = 2046.50M  (encrypted)"),
            Some(1_572_864)
        );
        assert_eq!(
            parse_swapusage("total = 0.00M  used = 0.00M  free = 0.00M  (encrypted)"),
            Some(0)
        );
        assert_eq!(parse_swapusage("nonsense"), None);
    }

    #[test]
    fn parses_psi_meminfo_and_stat() {
        let psi = parse_psi(
            "some avg10=1.50 avg60=0.20 avg300=0.00 total=1234\nfull avg10=0.25 avg60=0.00 avg300=0.00 total=99\n",
        );
        assert_eq!(psi.some_avg10, Some(1.5));
        assert_eq!(psi.full_avg10, Some(0.25));
        assert_eq!(
            parse_meminfo_swap(
                "MemTotal: 16000000 kB\nSwapTotal:  4194300 kB\nSwapFree:   4094300 kB\n"
            ),
            Some(100_000 * 1024)
        );
        assert_eq!(
            parse_proc_stat("cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3 4\n"),
            Some((150, 1000))
        );
    }

    #[test]
    fn sums_a_process_tree() {
        let procs = parse_ps(
            "    1     0  1000   0.0\n  100     1   200   5.0\n  101   100   300  50,5\n  102   101   400  10.0\n  200     1  9999   0.0\n",
        );
        assert_eq!(procs.len(), 5);
        assert!((procs[2].cpu_percent - 50.5).abs() < f64::EPSILON);
        assert_eq!(tree_rss_bytes(&procs, 100), (200 + 300 + 400) * 1024);
        assert_eq!(tree_rss_bytes(&procs, 4242), 0);
    }

    #[test]
    fn percentiles_by_nearest_rank() {
        let values: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(values.clone(), 99.0), Some(99.0));
        assert_eq!(percentile(values.clone(), 95.0), Some(95.0));
        assert_eq!(percentile(values, 100.0), Some(100.0));
        assert_eq!(percentile(vec![], 50.0), None);
        assert_eq!(percentile(vec![7.0], 99.0), Some(7.0));
    }

    #[test]
    fn the_canary_measures_lateness() {
        let canary = Canary::start(|| Command::new("true"), Instant::now());
        std::thread::sleep(Duration::from_millis(120));
        let readings = canary.finish();
        assert!(readings.lateness_us.len() >= 5, "{readings:?}");
        assert_eq!(readings.list_ms.len(), 1);
        let failing = Canary::start(|| Command::new("false"), Instant::now());
        std::thread::sleep(Duration::from_millis(120));
        let readings = failing.finish();
        assert_eq!(readings.list_ms, Vec::new());
        assert_eq!(readings.list_failures.len(), 1);
    }
}

/// The daemon's own disk figures (#272), read with `reef daemon status`
/// every 10 seconds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonDiskSample {
    /// Milliseconds since the scenario started.
    pub t_ms: u64,
    /// `ok`, `warn` or `low`.
    pub level: String,
    /// The lowest usable free space over the daemon's volumes (purgeable
    /// space counted on macOS).
    pub free_bytes: u64,
    /// That volume's floor.
    pub floor_bytes: u64,
    /// What Reef holds: workspaces, bases, stores and its state.
    pub reef_bytes: u64,
    /// Of it, the workspaces' unshared bytes.
    pub workspaces_bytes: u64,
}

/// How often [`DiskWatch`] reads the daemon's figures.
const DISK_EVERY: Duration = Duration::from_secs(10);

/// Reads `reef daemon status -o json`'s `disk` in a thread of its own.
#[derive(Debug)]
pub struct DiskWatch {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Vec<DaemonDiskSample>>>,
}

impl DiskWatch {
    /// Starts reading; `status` builds the `reef daemon status` command.
    pub fn start(status: impl Fn() -> Command + Send + 'static, epoch: Instant) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let read = |out: &mut Vec<DaemonDiskSample>| {
                if let Ok(output) = status().stdin(Stdio::null()).stderr(Stdio::null()).output()
                    && let Some(sample) = daemon_disk(&output.stdout, millis(epoch.elapsed()))
                {
                    out.push(sample);
                }
            };
            let mut out = Vec::new();
            while !flag.load(Ordering::Relaxed) {
                let started = Instant::now();
                read(&mut out);
                let mut left = DISK_EVERY.saturating_sub(started.elapsed());
                while !left.is_zero() && !flag.load(Ordering::Relaxed) {
                    let nap = left.min(Duration::from_millis(100));
                    std::thread::sleep(nap);
                    left = left.saturating_sub(nap);
                }
            }
            // The last reading follows the stop, so it is taken after the
            // run's release and `ws gc` however the 10 s fell.
            read(&mut out);
            out
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Stops, takes a last reading, and returns the readings.
    pub fn finish(mut self) -> Vec<DaemonDiskSample> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default()
    }
}

/// One reading from `reef daemon status -o json`; `None` without a `disk`
/// object (a daemon before #272).
pub fn daemon_disk(json: &[u8], t_ms: u64) -> Option<DaemonDiskSample> {
    let status: serde_json::Value = serde_json::from_slice(json).ok()?;
    let disk = status.get("disk")?;
    let number = |value: &serde_json::Value, key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    let tightest = disk
        .get("volumes")?
        .as_array()?
        .iter()
        .map(|volume| {
            let free = number(volume, "free_bytes").max(number(volume, "purgeable_free_bytes"));
            (free, number(volume, "floor_bytes"))
        })
        .min_by_key(|(free, floor)| i128::from(*free) - i128::from(*floor))?;
    Some(DaemonDiskSample {
        t_ms,
        level: disk.get("level")?.as_str()?.to_owned(),
        free_bytes: tightest.0,
        floor_bytes: tightest.1,
        reef_bytes: number(disk, "reef_bytes"),
        workspaces_bytes: number(disk, "workspaces_bytes"),
    })
}
