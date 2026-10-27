//! Shared by the load tests (`load.rs`, R1.23; `subscription_load.rs`,
//! R2.22): sampling the daemon's process, the machine, the report file and
//! the daemon's log beside it, and the arithmetic the reports use.

#![allow(
    clippy::cast_precision_loss,
    reason = "a measurement program's reports; the figures are far below 2^52"
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use super::spawned::SpawnedDaemon;

/// Samples the daemon's thread count and resident size every two seconds.
pub struct Monitor {
    threads: Arc<AtomicU64>,
    rss: Arc<AtomicU64>,
    stop: tokio_util::sync::CancellationToken,
}

impl Monitor {
    pub fn start(pid: u32) -> Monitor {
        let monitor = Monitor {
            threads: Arc::new(AtomicU64::new(0)),
            rss: Arc::new(AtomicU64::new(0)),
            stop: tokio_util::sync::CancellationToken::new(),
        };
        let (threads, rss, stop) = (
            Arc::clone(&monitor.threads),
            Arc::clone(&monitor.rss),
            monitor.stop.clone(),
        );
        tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! {
                    () = stop.cancelled() => return,
                    _ = every.tick() => {}
                }
                let (now_threads, now_rss) =
                    tokio::task::spawn_blocking(move || process_usage(pid))
                        .await
                        .unwrap();
                threads.fetch_max(now_threads, Ordering::Relaxed);
                rss.fetch_max(now_rss, Ordering::Relaxed);
            }
        });
        monitor
    }

    pub fn stop(&self) {
        self.stop.cancel();
    }

    pub fn peak_threads(&self) -> u64 {
        self.threads.load(Ordering::Relaxed)
    }

    pub fn peak_rss(&self) -> u64 {
        self.rss.load(Ordering::Relaxed)
    }
}

/// `pid`'s thread count and resident bytes now (0 if unreadable).
pub fn process_usage(pid: u32) -> (u64, u64) {
    if cfg!(target_os = "linux") {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
                .unwrap_or(0)
        };
        (field("Threads:"), field("VmRSS:") * 1024)
    } else {
        let pid = pid.to_string();
        let threads = command_output("ps", &["-M", "-p", &pid])
            .lines()
            .count()
            .saturating_sub(1) as u64;
        let rss = command_output("ps", &["-o", "rss=", "-p", &pid])
            .trim()
            .parse::<u64>()
            .unwrap_or(0)
            * 1024;
        (threads, rss)
    }
}

/// The CPU time `pid` has used, user and system, in seconds (0 if
/// unreadable).
pub fn process_cpu_secs(pid: u32) -> f64 {
    if cfg!(target_os = "linux") {
        // Fields 14 and 15 of /proc/<pid>/stat, after the parenthesized
        // command, in clock ticks (100 a second on every Linux Reef runs on).
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let Some((_, rest)) = stat.rsplit_once(')') else {
            return 0.0;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let ticks = |index: usize| {
            fields
                .get(index)
                .and_then(|field| field.parse::<u64>().ok())
                .unwrap_or(0)
        };
        // `rest` starts at field 3 (the state).
        (ticks(11) + ticks(12)) as f64 / 100.0
    } else {
        // `[[dd-]hh:]mm:ss.ss`.
        let text = command_output("ps", &["-o", "time=", "-p", &pid.to_string()]);
        let text = text.trim();
        let (days, clock) = text.split_once('-').unwrap_or(("0", text));
        let mut secs = days.parse::<f64>().unwrap_or(0.0) * 86_400.0;
        let mut unit = 1.0;
        for part in clock.rsplit(':') {
            secs += part.parse::<f64>().unwrap_or(0.0) * unit;
            unit *= 60.0;
        }
        secs
    }
}

/// How many descriptors `pid` has open (0 if unreadable).
pub fn open_descriptors(pid: u32) -> u64 {
    if cfg!(target_os = "linux") {
        std::fs::read_dir(format!("/proc/{pid}/fd")).map_or(0, |dir| dir.count() as u64)
    } else {
        command_output("lsof", &["-n", "-P", "-p", &pid.to_string()])
            .lines()
            .count()
            .saturating_sub(1) as u64
    }
}

pub fn command_output(program: &str, args: &[&str]) -> String {
    std::process::Command::new(program)
        .args(args)
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
}

/// Where the program runs.
pub fn machine() -> Value {
    let (model, memory) = if cfg!(target_os = "linux") {
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let model = cpuinfo
            .lines()
            .find_map(|line| line.strip_prefix("model name"))
            .map(|rest| rest.trim_start_matches([' ', '\t', ':']).to_owned());
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let memory = meminfo
            .lines()
            .find_map(|line| line.strip_prefix("MemTotal:"))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            .map(|kb| kb * 1024);
        (model, memory)
    } else {
        let model = command_output("sysctl", &["-n", "machdep.cpu.brand_string"]);
        let memory = command_output("sysctl", &["-n", "hw.memsize"])
            .trim()
            .parse()
            .ok();
        (Some(model.trim().to_owned()), memory)
    };
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cpus": std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
        "model": model,
        "memory_bytes": memory,
        "kernel": command_output("uname", &["-sr"]).trim(),
        "ci": std::env::var("GITHUB_ACTIONS").is_ok(),
        "runner": std::env::var("RUNNER_NAME").ok(),
    })
}

/// Writes `report` to `target/load/<name>-<stamp>.json` (the stamp without
/// its separators) and returns the path.
pub fn write_report(report: &Value, name: &str, stamp: &str) -> PathBuf {
    let target = Path::new(env!("CARGO_BIN_EXE_reef-daemon"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let dir = target.join("load");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}-{}.json", stamp.replace([':', '-'], "")));
    std::fs::write(&path, serde_json::to_string_pretty(report).unwrap() + "\n").unwrap();
    path
}

/// Copies the daemon's log files beside the report, into
/// `<report>.daemon-log/`, before the state directory goes.
pub fn keep_daemon_log(daemon: &SpawnedDaemon, report: &Path) {
    let dir = report.with_extension("daemon-log");
    std::fs::create_dir_all(&dir).unwrap();
    for file in super::log_files(&daemon.state_dir) {
        std::fs::copy(&file, dir.join(file.file_name().unwrap())).unwrap();
    }
}

/// The `key=value` fields of the daemon's log lines whose message contains
/// `message` (the per-minute summaries), oldest first; values that are not
/// numbers are left out.
pub fn summary_lines(daemon: &SpawnedDaemon, message: &str) -> Vec<HashMap<String, u64>> {
    daemon
        .log_lines(message)
        .iter()
        .filter_map(|event| event["message"].as_str().map(str::to_owned))
        .map(|message| {
            message
                .split_whitespace()
                .filter_map(|field| {
                    let (key, value) = field.split_once('=')?;
                    Some((key.to_owned(), value.parse().ok()?))
                })
                .collect()
        })
        .collect()
}

/// Raises this process's soft `RLIMIT_NOFILE` as the daemon does, so a
/// test that opens hundreds of connections (and the daemon it spawns,
/// which inherits the limit) is not stopped by a shell's default. Returns
/// the limit before and after.
pub fn raise_open_file_limit() -> (u64, u64) {
    reef_daemon::limits::raise_open_file_limit().expect("raise RLIMIT_NOFILE")
}

/// The nearest-rank `p`th percentile of sorted `values`; 0 if empty.
pub fn percentile(values: &[u64], p: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let rank = (p * values.len()).div_ceil(100);
    values[rank.clamp(1, values.len()) - 1]
}

/// [`percentile`] in tenths of a percent: `per_mille(values, 999)` is the
/// p99.9.
pub fn per_mille(values: &[u64], p: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let rank = (p * values.len()).div_ceil(1000);
    values[rank.clamp(1, values.len()) - 1]
}

pub fn mean(values: &[u64]) -> f64 {
    values.iter().sum::<u64>() as f64 / values.len().max(1) as f64
}

pub fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub fn now_ms() -> i64 {
    reef_daemon::model::Timestamp::now().0
}

pub fn gb(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1 << 30)
}

pub fn mb(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1 << 20)
}

/// Now, as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn utc_stamp() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}
