//! `report.json`, `summary.md`, and the gates (#264's decision 4).
//!
//! Every gate is judged per repository, and only on the `standard` and
//! `linux` profiles: a `dev` run shows its numbers with the verdict `dev`,
//! and a repository that is not gated (Reef's own) shows `reported`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::disk::gib;
use super::outcomes;
use super::repos::Stack;
use super::sample::{Phase, Sample, percentile};
use super::{Host, Profile, TicketRun, Variant};

/// [`Report::schema`] for files this build writes.
pub const SCHEMA: u32 = 1;

/// `report.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// The file format's version.
    pub schema: u32,
    /// UTC, RFC 3339.
    pub started_at: String,
    /// The profile the run stands for.
    pub profile: Profile,
    /// Why the run was judged as `dev` though another profile was asked
    /// for: the host is not that profile's machine.
    #[serde(default)]
    pub profile_note: Option<String>,
    /// What it varied.
    pub variant: Variant,
    /// The machine.
    pub host: Host,
    /// Tickets per repository.
    pub tickets: u32,
    /// How long the tickets stayed idle before release, seconds.
    pub idle_secs: u64,
    /// One entry per repository, in run order.
    pub repos: Vec<RepoReport>,
}

/// One repository's scenario.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RepoReport {
    /// Its name in `repos.toml`.
    pub name: String,
    /// Its stack.
    pub stack: Option<Stack>,
    /// Where it was cloned from.
    pub url: String,
    /// The commit.
    pub rev: String,
    /// Whether its gates are judged.
    pub gated: bool,
    /// Why the scenario stopped early, if it did.
    pub error: Option<String>,
    /// The volumes watched: the workspace root's first.
    pub volumes: Vec<String>,
    /// Each volume's size, bytes.
    pub volume_bytes: Vec<u64>,
    /// The seed's source run (`--seed` other than `off`).
    pub seed: Option<TicketRun>,
    /// One run per solo ticket.
    pub solo: Vec<TicketRun>,
    /// Five at once: each ticket's rounds 1 and 2.
    pub five: Vec<TicketRun>,
    /// The scenario's spans.
    pub phases: Vec<Phase>,
    /// Once a second.
    pub samples: Vec<Sample>,
    /// The canary's `ws list` latencies: (`t_ms`, ms).
    pub list_ms: Vec<(u64, u64)>,
    /// When the canary's `ws list` failed or timed out (`t_ms`).
    #[serde(default)]
    pub list_failures: Vec<u64>,
    /// The canary's sleep lateness p99 per phase, µs.
    pub lateness_p99_us: BTreeMap<String, u32>,
    /// What Reef kept on purpose, grown since the scenario started: its
    /// bases and stores, bytes by name.
    pub kept: BTreeMap<String, u64>,
    /// How much the repository's shared tool caches (`GOCACHE`, yarn's)
    /// grew during the scenario, bytes by name: counted as used by G4.
    #[serde(default)]
    pub caches: BTreeMap<String, u64>,
    /// Creates `ws run` reported refused.
    pub creates_refused: u32,
    /// The figures the gates read.
    pub summary: Summary,
}

/// The figures a repository's gates read, computed from its samples.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// Disk consumed at peak while solo ticket 1 ran, bytes.
    pub solo_peak_bytes: Option<u64>,
    /// Disk consumed at peak from the five tickets' start to their release.
    pub five_peak_bytes: Option<u64>,
    /// The least free space seen on the workspace root's volume.
    pub min_free_bytes: Option<u64>,
    /// The floor (decision 7): the larger of 20 GiB and 5% of the volume.
    pub floor_bytes: u64,
    /// Start free minus final free, after release and `ws gc`; negative
    /// when more is free than at the start.
    pub after_bytes: Option<i64>,
    /// The sum of [`RepoReport::kept`].
    pub kept_bytes: u64,
    /// Logical cores.
    pub cores: u32,
    /// The 1-minute load's p95 during the five tickets.
    pub load_p95: Option<f64>,
    /// Its maximum.
    pub load_max: Option<f64>,
    /// The mean CPU busy share during the five tickets.
    pub cpu_busy_mean: Option<f64>,
    /// macOS: seconds at warn pressure or worse.
    pub pressure_warn_secs: Option<u64>,
    /// macOS: seconds at critical pressure.
    pub pressure_critical_secs: Option<u64>,
    /// macOS: bytes swapped out during the scenario.
    pub swapout_bytes: Option<u64>,
    /// The most swap in use.
    pub swap_used_max_bytes: Option<u64>,
    /// Linux: PSI `memory full avg10`'s maximum, percent.
    pub psi_full_max: Option<f64>,
    /// The canary's `ws list` p99 during the five tickets, ms.
    pub list_p99_ms: Option<f64>,
    /// The canary's `ws list` calls that failed or timed out during the
    /// five tickets.
    #[serde(default)]
    pub list_failures: u32,
    /// The canary's sleep lateness p99 during the five tickets, ms.
    pub lateness_p99_ms: Option<f64>,
    /// Solo ticket 1's wall time, ms.
    pub solo_ms: Option<u64>,
    /// The median five-at-once ticket's round 1, ms.
    pub median_ticket_ms: Option<u64>,
    /// The five tickets' makespan, both rounds, ms.
    pub makespan_ms: Option<u64>,
    /// Builds whose log shows `ENOSPC`.
    pub enospc_builds: u32,
    /// Tickets whose round-1 outcomes differ from the same patch alone,
    /// with how.
    pub mismatches: BTreeMap<u32, Vec<String>>,
    /// Tickets with a solo reference to compare against.
    pub compared: u32,
}

const GIB: u64 = 1 << 30;
/// The floor's least value.
pub const FLOOR_MIN: u64 = 20 * GIB;
/// G2: five tickets at peak.
pub const G2_PEAK: u64 = 100 * GIB;
/// G4: left over after release and gc, beyond what is kept on purpose.
pub const G4_AFTER: u64 = 2 * GIB;
/// G6: swap-outs over the run.
pub const G6_SWAPOUT: u64 = 2 * GIB;
/// G6 on Linux: PSI `memory full avg10`.
pub const G6_PSI_FULL: f64 = 10.0;

/// The floor for a volume of `volume_bytes` (decision 7).
pub fn floor(volume_bytes: u64) -> u64 {
    FLOOR_MIN.max(volume_bytes / 20)
}

/// G3's limit for a stack: 3× for Rust (linked test binaries are per
/// workspace whatever is shared), 2× for the rest (the issue's number).
pub fn g3_limit(stack: Option<Stack>) -> f64 {
    if stack == Some(Stack::Rust) { 3.0 } else { 2.0 }
}

/// Computes [`RepoReport::summary`] from its other fields.
#[allow(
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    reason = "means and ratios; one field after another"
)]
pub fn summarize(repo: &RepoReport, cores: u32) -> Summary {
    let phase = |name: &str| repo.phases.iter().find(|p| p.name == name);
    let in_span = |s: &Sample, start: u64, end: u64| s.t_ms >= start && s.t_ms <= end;
    // The bytes consumed at peak within [start, end], against the free
    // space at `start`, summed over volumes.
    let peak = |start: u64, end: u64| -> Option<u64> {
        let base = repo
            .samples
            .iter()
            .rfind(|s| s.t_ms <= start)
            .or_else(|| repo.samples.first())?;
        repo.samples
            .iter()
            .filter(|s| in_span(s, start, end))
            .map(|s| {
                base.free_bytes
                    .iter()
                    .zip(&s.free_bytes)
                    .map(|(b, n)| b.saturating_sub(*n))
                    .sum::<u64>()
            })
            .max()
    };
    let five_span = phase("five").map(|five| {
        let end = phase("idle").map_or(five.end_ms, |idle| idle.end_ms);
        (five.start_ms, end)
    });
    let busy: Vec<&Sample> = match phase("five") {
        Some(five) => repo
            .samples
            .iter()
            .filter(|s| in_span(s, five.start_ms, five.end_ms))
            .collect(),
        None => vec![],
    };
    let loads: Vec<f64> = busy.iter().map(|s| s.load1).collect();
    let cpu: Vec<f64> = busy.iter().filter_map(|s| s.cpu_busy).collect();
    let levels: Vec<u32> = repo
        .samples
        .iter()
        .filter_map(|s| s.pressure_level)
        .collect();
    let secs_at = |at_least: u32| -> Option<u64> {
        (!levels.is_empty()).then(|| levels.iter().filter(|l| **l >= at_least).count() as u64)
    };
    let swapouts: Vec<u64> = repo
        .samples
        .iter()
        .filter_map(|s| s.swapout_bytes)
        .collect();
    let list: Vec<f64> = match phase("five") {
        Some(five) => repo
            .list_ms
            .iter()
            .filter(|(t, _)| *t >= five.start_ms && *t <= five.end_ms)
            .map(|(_, ms)| *ms as f64)
            .collect(),
        None => vec![],
    };
    let mut round1: Vec<u64> = repo
        .five
        .iter()
        .filter(|t| t.round == 1)
        .map(|t| t.wall_ms)
        .collect();
    round1.sort_unstable();
    let mut mismatches = BTreeMap::new();
    let mut compared = 0;
    for run in repo.five.iter().filter(|t| t.round == 1) {
        let Some(solo) = repo.solo.iter().find(|s| s.ticket == run.ticket) else {
            continue;
        };
        compared += 1;
        let mut diff = outcomes::differences(&solo.outcomes, &run.outcomes);
        if solo.passed() != run.passed() {
            diff.push(format!(
                "exit {:?} alone, {:?} five at once",
                solo.exit_code, run.exit_code
            ));
        }
        if !diff.is_empty() {
            mismatches.insert(run.ticket, diff);
        }
    }
    let first_free = repo.samples.first().and_then(|s| s.free_bytes.first());
    let last_free = repo.samples.last().and_then(|s| s.free_bytes.first());
    let kept_bytes = repo.kept.values().sum();
    Summary {
        solo_peak_bytes: phase("solo-1").and_then(|p| peak(p.start_ms, p.end_ms)),
        five_peak_bytes: five_span.and_then(|(start, end)| peak(start, end)),
        min_free_bytes: repo
            .samples
            .iter()
            .filter_map(|s| s.free_bytes.first().copied())
            .min(),
        floor_bytes: floor(repo.volume_bytes.first().copied().unwrap_or(0)),
        after_bytes: first_free.zip(last_free).map(|(first, last)| {
            i64::try_from(*first).unwrap_or(i64::MAX) - i64::try_from(*last).unwrap_or(i64::MAX)
        }),
        kept_bytes,
        cores,
        load_p95: percentile(loads.clone(), 95.0),
        load_max: loads.iter().copied().reduce(f64::max),
        cpu_busy_mean: (!cpu.is_empty()).then(|| cpu.iter().sum::<f64>() / cpu.len() as f64),
        pressure_warn_secs: secs_at(2),
        pressure_critical_secs: secs_at(4),
        swapout_bytes: swapouts
            .first()
            .zip(swapouts.last())
            .map(|(a, b)| b.saturating_sub(*a)),
        swap_used_max_bytes: repo.samples.iter().filter_map(|s| s.swap_used_bytes).max(),
        psi_full_max: repo
            .samples
            .iter()
            .filter_map(|s| s.psi_full_avg10)
            .reduce(f64::max),
        list_p99_ms: percentile(list, 99.0),
        list_failures: match phase("five") {
            Some(five) => u32::try_from(
                repo.list_failures
                    .iter()
                    .filter(|t| **t >= five.start_ms && **t <= five.end_ms)
                    .count(),
            )
            .unwrap_or(u32::MAX),
            None => 0,
        },
        lateness_p99_ms: repo
            .lateness_p99_us
            .get("five")
            .map(|us| f64::from(*us) / 1000.0),
        solo_ms: repo.solo.iter().find(|t| t.ticket == 1).map(|t| t.wall_ms),
        median_ticket_ms: round1.get(round1.len().saturating_sub(1) / 2).copied(),
        makespan_ms: phase("five").map(|p| p.end_ms.saturating_sub(p.start_ms)),
        enospc_builds: u32::try_from(
            repo.solo
                .iter()
                .chain(&repo.five)
                .chain(&repo.seed)
                .filter(|t| t.enospc)
                .count(),
        )
        .unwrap_or(u32::MAX),
        mismatches,
        compared,
    }
}

/// A gate's verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Met.
    Pass,
    /// Missed.
    Fail,
    /// The run lacks what the gate reads.
    NotMeasured,
    /// The repository is reported, not gated.
    Reported,
    /// A `dev` profile run: shown, never judged.
    Dev,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "**fail**",
            Verdict::NotMeasured => "not measured",
            Verdict::Reported => "reported",
            Verdict::Dev => "dev",
        })
    }
}

/// One gate's result for one repository.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    /// `G1` to `G9`.
    pub id: &'static str,
    /// What it guards.
    pub name: &'static str,
    /// The threshold, in words.
    pub threshold: String,
    /// What the run measured.
    pub value: String,
    /// The verdict.
    pub verdict: Verdict,
}

fn met(ok: Option<bool>) -> Verdict {
    match ok {
        Some(true) => Verdict::Pass,
        Some(false) => Verdict::Fail,
        None => Verdict::NotMeasured,
    }
}

fn opt<T>(value: Option<T>, show: impl Fn(T) -> String) -> String {
    value.map_or_else(|| "n/a".to_owned(), show)
}

/// Judges one repository's gates.
#[allow(
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    reason = "one block per gate; ratios"
)]
pub fn gates(repo: &RepoReport, profile: Profile, os: &str) -> Vec<Gate> {
    let s = &repo.summary;
    let cores = f64::from(s.cores.max(1));
    let linux = os == "linux";
    let mut out = vec![
        Gate {
            id: "G1",
            name: "disk safety",
            threshold: format!(
                "no refused create or ENOSPC; free ≥ floor ({:.0} GiB)",
                gib(s.floor_bytes)
            ),
            value: format!(
                "{} refused, {} ENOSPC, min free {}",
                repo.creates_refused,
                s.enospc_builds,
                opt(s.min_free_bytes, |b| format!("{:.1} GiB", gib(b)))
            ),
            verdict: met(s.min_free_bytes.map(|free| {
                repo.creates_refused == 0 && s.enospc_builds == 0 && free >= s.floor_bytes
            })),
        },
        Gate {
            id: "G2",
            name: "disk peak",
            threshold: format!("five tickets ≤ {:.0} GiB", gib(G2_PEAK)),
            value: opt(s.five_peak_bytes, |b| format!("{:.2} GiB", gib(b))),
            verdict: met(s.five_peak_bytes.map(|b| b <= G2_PEAK)),
        },
    ];
    let limit = g3_limit(repo.stack);
    let ratio = s
        .five_peak_bytes
        .zip(s.solo_peak_bytes.filter(|b| *b > 0))
        .map(|(five, one)| five as f64 / one as f64);
    out.push(Gate {
        id: "G3",
        name: "disk scaling",
        threshold: format!("five ≤ {limit:.0}× one"),
        value: opt(ratio, |r| {
            format!(
                "{r:.2}× ({:.2} / {:.2} GiB)",
                gib(s.five_peak_bytes.unwrap_or(0)),
                gib(s.solo_peak_bytes.unwrap_or(0))
            )
        }),
        verdict: met(ratio.map(|r| r <= limit)),
    });
    let left = s
        .after_bytes
        .map(|after| after - i64::try_from(s.kept_bytes).unwrap_or(i64::MAX));
    out.push(Gate {
        id: "G4",
        name: "disk after",
        threshold: format!("≤ {:.0} GiB beyond Reef's bases and stores", gib(G4_AFTER)),
        value: opt(s.after_bytes, |after| {
            format!(
                "{:.2} GiB used, {:.2} GiB Reef kept, {:.2} GiB tool caches grew",
                signed_gib(after),
                gib(s.kept_bytes),
                gib(repo.caches.values().sum())
            )
        }),
        verdict: met(left.map(|l| l <= i64::try_from(G4_AFTER).unwrap_or(i64::MAX))),
    });
    out.push(Gate {
        id: "G5",
        name: "load",
        threshold: format!(
            "p95 ≤ {:.0}, max ≤ {:.0} ({} cores)",
            2.0 * cores,
            3.0 * cores,
            s.cores
        ),
        value: format!(
            "p95 {}, max {}",
            opt(s.load_p95, |l| format!("{l:.1}")),
            opt(s.load_max, |l| format!("{l:.1}"))
        ),
        verdict: met(s
            .load_p95
            .zip(s.load_max)
            .map(|(p95, max)| p95 <= 2.0 * cores && max <= 3.0 * cores)),
    });
    out.push(if linux {
        Gate {
            id: "G6",
            name: "memory",
            threshold: format!("PSI memory full avg10 ≤ {G6_PSI_FULL:.0}%"),
            value: format!(
                "full max {}, swap max {}",
                opt(s.psi_full_max, |p| format!("{p:.1}%")),
                opt(s.swap_used_max_bytes, |b| format!("{:.2} GiB", gib(b)))
            ),
            verdict: met(s.psi_full_max.map(|p| p <= G6_PSI_FULL)),
        }
    } else {
        Gate {
            id: "G6",
            name: "memory",
            threshold: format!(
                "no critical pressure; swap-outs ≤ {:.0} GiB",
                gib(G6_SWAPOUT)
            ),
            value: format!(
                "critical {} s, warn {} s, swap-outs {}",
                opt(s.pressure_critical_secs, |n| n.to_string()),
                opt(s.pressure_warn_secs, |n| n.to_string()),
                opt(s.swapout_bytes, |b| format!("{:.2} GiB", gib(b)))
            ),
            verdict: met(s
                .pressure_critical_secs
                .zip(s.swapout_bytes)
                .map(|(critical, swap)| critical == 0 && swap <= G6_SWAPOUT)),
        }
    });
    out.push(Gate {
        id: "G7",
        name: "usability",
        threshold: "`ws list` p99 < 1 s, none failed; 10 ms sleep late p99 < 50 ms".to_owned(),
        value: format!(
            "list p99 {}, {} failed, late p99 {}",
            opt(s.list_p99_ms, |ms| format!("{ms:.0} ms")),
            s.list_failures,
            opt(s.lateness_p99_ms, |ms| format!("{ms:.1} ms"))
        ),
        verdict: met(s
            .list_p99_ms
            .zip(s.lateness_p99_ms)
            .map(|(list, late)| list < 1000.0 && late < 50.0 && s.list_failures == 0)),
    });
    let slowdown = s
        .median_ticket_ms
        .zip(s.solo_ms.filter(|ms| *ms > 0))
        .map(|(median, solo)| median as f64 / solo as f64);
    out.push(Gate {
        id: "G8",
        name: "throughput",
        threshold: "median ticket ≤ 5× solo (the limit's 1.1× makespan needs #271)".to_owned(),
        value: opt(slowdown, |r| {
            format!(
                "{r:.2}× (median {:.0} s, solo {:.0} s, makespan {:.0} s)",
                s.median_ticket_ms.unwrap_or(0) as f64 / 1000.0,
                s.solo_ms.unwrap_or(0) as f64 / 1000.0,
                s.makespan_ms.unwrap_or(0) as f64 / 1000.0
            )
        }),
        verdict: met(slowdown.map(|r| r <= 5.0)),
    });
    out.push(Gate {
        id: "G9",
        name: "correctness",
        threshold: "each ticket's outcomes equal its solo run's".to_owned(),
        value: if s.compared == 0 {
            "no solo reference".to_owned()
        } else {
            format!("{} of {} differ", s.mismatches.len(), s.compared)
        },
        verdict: met((s.compared > 0).then_some(s.mismatches.is_empty())),
    });
    if repo.error.is_some() {
        for gate in &mut out {
            if gate.verdict == Verdict::Pass {
                gate.verdict = Verdict::NotMeasured;
            }
        }
    }
    for gate in &mut out {
        if !profile.judged() {
            gate.verdict = Verdict::Dev;
        } else if !repo.gated {
            gate.verdict = Verdict::Reported;
        }
    }
    out
}

/// Re-reads each run's log under `dir` (the report's directory) and
/// recomputes its outcomes and every summary: for a report written by an
/// older parser. A missing log leaves that run as it was.
pub fn reparse(report: &mut Report, dir: &std::path::Path) {
    let cores = report.host.cores;
    for repo in &mut report.repos {
        for t in repo
            .solo
            .iter_mut()
            .chain(repo.five.iter_mut())
            .chain(repo.seed.iter_mut())
        {
            if let Ok(text) = std::fs::read_to_string(dir.join(&t.log)) {
                t.outcomes = outcomes::parse(&text);
                t.units_compiled = outcomes::units_compiled(&text);
                t.enospc = outcomes::enospc(&text);
            }
        }
        repo.summary = summarize(repo, cores);
    }
}

/// Whether any gate in `report` failed (what `--gate` exits 1 on).
pub fn any_failed(report: &Report) -> bool {
    report.repos.iter().any(|repo| {
        gates(repo, report.profile, &report.host.os)
            .iter()
            .any(|g| g.verdict == Verdict::Fail)
            || (report.profile.judged() && repo.gated && repo.error.is_some())
    })
}

#[allow(clippy::cast_precision_loss, reason = "display")]
fn secs(ms: u64) -> String {
    format!("{:.1}", ms as f64 / 1000.0)
}

/// `summary.md`.
#[allow(clippy::too_many_lines, reason = "one table after another")]
pub fn render(report: &Report) -> String {
    let host = &report.host;
    let mut md = String::new();
    let _ = writeln!(md, "# Fleet benchmark: {}\n", report.started_at);
    let _ = writeln!(
        md,
        "Profile **{}**{} · {} · {} tickets, {} s idle · {}\n",
        report.profile,
        if report.profile.judged() {
            ""
        } else {
            " (not judged)"
        },
        report.variant,
        report.tickets,
        report.idle_secs,
        host.reef_version
    );
    let _ = writeln!(
        md,
        "Host: {} {}, {} cores{}, {:.0} GiB memory, {:.0} GiB volume. Tools: {}.",
        host.os,
        host.arch,
        host.cores,
        host.performance_cores
            .map(|p| format!(" ({p} performance)"))
            .unwrap_or_default(),
        gib(host.memory_bytes),
        gib(host.volume_bytes),
        host.tools
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if let Some(note) = &report.profile_note {
        let _ = writeln!(md, "\n**Judged as `dev`**: {note}.");
    }
    if !host.local_snapshots.is_empty() || host.spotlight.is_some() {
        let _ = writeln!(
            md,
            "Time Machine local snapshots: {}. Spotlight: {}.",
            host.local_snapshots.len(),
            host.spotlight.as_deref().unwrap_or("unknown")
        );
    }
    for repo in &report.repos {
        let _ = writeln!(
            md,
            "\n## {} ({}{})\n",
            repo.name,
            repo.stack
                .map_or("?".to_owned(), |s| format!("{s:?}").to_lowercase()),
            if repo.gated { "" } else { ", reported" }
        );
        let _ = writeln!(md, "`{}` at `{}`\n", repo.url, short(&repo.rev));
        if let Some(error) = &repo.error {
            let _ = writeln!(md, "**Stopped early**: {error}\n");
        }
        let _ = writeln!(md, "| Gate | Threshold | Measured | Verdict |");
        let _ = writeln!(md, "| --- | --- | --- | --- |");
        for gate in gates(repo, report.profile, &host.os) {
            let _ = writeln!(
                md,
                "| {} {} | {} | {} | {} |",
                gate.id, gate.name, gate.threshold, gate.value, gate.verdict
            );
        }
        let _ = writeln!(
            md,
            "\n| Run | Ticket | Round | Exit | Wall (s) | Units | Peak RSS (GiB) | Build (GiB) | Build unshared (GiB) | Other unshared (GiB) |"
        );
        let _ = writeln!(
            md,
            "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"
        );
        let rows = repo
            .seed
            .iter()
            .map(|t| ("seed", t))
            .chain(repo.solo.iter().map(|t| ("solo", t)))
            .chain(repo.five.iter().map(|t| ("five", t)));
        for (run, t) in rows {
            let disk = t.disk.as_ref();
            let _ = writeln!(
                md,
                "| {run} | {} | {} | {} | {} | {} | {:.2} | {} | {} | {} |",
                t.ticket,
                t.round,
                t.exit_code
                    .map_or_else(|| "error".to_owned(), |c| c.to_string()),
                secs(t.wall_ms),
                opt(t.units_compiled, |u| u.to_string()),
                gib(t.peak_rss_bytes),
                opt(disk, |d| format!("{:.2}", gib(d.build.allocated_bytes))),
                opt(disk.and_then(|d| d.build.unshared_bytes), |b| format!(
                    "{:.2}",
                    gib(b)
                )),
                opt(disk.and_then(|d| d.other.unshared_bytes), |b| format!(
                    "{:.2}",
                    gib(b)
                )),
            );
        }
        let s = &repo.summary;
        let _ = writeln!(
            md,
            "\nDisk: solo peak {}, five peak {}, min free {}, after gc {} (Reef kept {}: {}; tool caches grew {}).",
            opt(s.solo_peak_bytes, |b| format!("{:.2} GiB", gib(b))),
            opt(s.five_peak_bytes, |b| format!("{:.2} GiB", gib(b))),
            opt(s.min_free_bytes, |b| format!("{:.1} GiB", gib(b))),
            opt(s.after_bytes, |b| format!("{:.2} GiB", signed_gib(b))),
            format_args!("{:.2} GiB", gib(s.kept_bytes)),
            repo.kept
                .iter()
                .map(|(k, v)| format!("{k} {:.2} GiB", gib(*v)))
                .collect::<Vec<_>>()
                .join(", "),
            if repo.caches.is_empty() {
                "nothing".to_owned()
            } else {
                repo.caches
                    .iter()
                    .map(|(k, v)| format!("{k} {:.2} GiB", gib(*v)))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
        let _ = writeln!(
            md,
            "CPU busy (five): {}. Swap max: {}.",
            opt(s.cpu_busy_mean, |c| format!("{:.0}%", c * 100.0)),
            opt(s.swap_used_max_bytes, |b| format!("{:.2} GiB", gib(b)))
        );
        for (run, t) in repo
            .seed
            .iter()
            .map(|t| ("seed", t))
            .chain(repo.solo.iter().map(|t| ("solo", t)))
            .chain(repo.five.iter().map(|t| ("five", t)))
        {
            if let Some(error) = &t.error {
                let _ = writeln!(
                    md,
                    "\n{run} ticket {} round {}: `reef ws run` failed: {}",
                    t.ticket,
                    t.round,
                    error.chars().take(300).collect::<String>()
                );
            }
        }
        for (ticket, diff) in &s.mismatches {
            let _ = writeln!(md, "\nTicket {ticket} differs from its solo run:");
            for line in diff.iter().take(10) {
                let _ = writeln!(md, "- {line}");
            }
        }
    }
    md
}

#[allow(clippy::cast_precision_loss, reason = "a display figure")]
fn signed_gib(bytes: i64) -> f64 {
    bytes as f64 / f64::from(1_u32 << 30)
}

fn short(rev: &str) -> &str {
    rev.get(..12).unwrap_or(rev)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::outcomes::Status;
    use crate::fleet::{SeedMode, Toggle};

    fn sample(t_ms: u64, free_gib: u64, load1: f64) -> Sample {
        Sample {
            t_ms,
            free_bytes: vec![free_gib * GIB],
            load1,
            pressure_level: Some(1),
            swapout_bytes: Some(0),
            ..Sample::default()
        }
    }

    fn phase(name: &str, start_ms: u64, end_ms: u64) -> Phase {
        Phase {
            name: name.to_owned(),
            start_ms,
            end_ms,
        }
    }

    fn ticket(ticket: u32, round: u32, wall_ms: u64) -> TicketRun {
        TicketRun {
            ticket,
            round,
            exit_code: Some(0),
            wall_ms,
            outcomes: [(format!("cargo:c{ticket}:t"), Status::Pass)].into(),
            ..TicketRun::default()
        }
    }

    /// A repository that passes every gate on an 8-core, 512 GB machine.
    fn passing() -> RepoReport {
        let mut repo = RepoReport {
            name: "x".to_owned(),
            stack: Some(Stack::Go),
            gated: true,
            volume_bytes: vec![500 * GIB],
            samples: vec![
                sample(0, 150, 1.0),
                sample(1000, 140, 2.0),
                sample(2000, 150, 1.0),
                sample(3000, 140, 8.0),
                sample(4000, 138, 12.0),
                sample(5000, 149, 1.0),
            ],
            phases: vec![
                phase("solo-1", 500, 1500),
                phase("five", 2500, 4500),
                phase("idle", 4500, 4600),
            ],
            solo: (1..=5).map(|n| ticket(n, 1, 1000)).collect(),
            five: (1..=5).map(|n| ticket(n, 1, 2000)).collect(),
            list_ms: vec![(3000, 40), (4000, 60)],
            lateness_p99_us: [("five".to_owned(), 2000)].into(),
            kept: [("bases".to_owned(), GIB / 2)].into(),
            ..RepoReport::default()
        };
        repo.summary = summarize(&repo, 8);
        repo
    }

    fn report(profile: Profile, repo: RepoReport) -> Report {
        Report {
            schema: SCHEMA,
            started_at: "2026-11-12T00:00:00Z".to_owned(),
            profile,
            profile_note: None,
            variant: Variant {
                limit: Toggle::Off,
                seed: SeedMode::Off,
                stores: Toggle::Off,
            },
            host: Host {
                os: "macos".to_owned(),
                arch: "aarch64".to_owned(),
                cores: 8,
                performance_cores: Some(4),
                memory_bytes: 16 * GIB,
                volume_bytes: 500 * GIB,
                reef_version: "reef 0.4.2".to_owned(),
                tools: BTreeMap::new(),
                local_snapshots: vec![],
                spotlight: None,
            },
            tickets: 5,
            idle_secs: 0,
            repos: vec![repo],
        }
    }

    #[test]
    fn summarizes_peaks_against_each_phase_start() {
        let s = passing().summary;
        assert_eq!(s.solo_peak_bytes, Some(10 * GIB));
        assert_eq!(s.five_peak_bytes, Some(12 * GIB));
        assert_eq!(s.min_free_bytes, Some(138 * GIB));
        assert_eq!(s.after_bytes, Some(i64::try_from(GIB).unwrap()));
        assert_eq!(s.floor_bytes, 25 * GIB);
        assert_eq!(s.load_max, Some(12.0));
        assert_eq!(s.median_ticket_ms, Some(2000));
        assert_eq!(s.compared, 5);
        assert!(s.mismatches.is_empty());
    }

    #[test]
    fn a_passing_run_passes_every_gate() {
        let report = report(Profile::Standard, passing());
        let verdicts: Vec<_> = gates(&report.repos[0], report.profile, "macos")
            .into_iter()
            .map(|g| (g.id, g.verdict))
            .collect();
        assert!(
            verdicts.iter().all(|(_, v)| *v == Verdict::Pass),
            "{verdicts:?}"
        );
        assert!(!any_failed(&report));
        let md = render(&report);
        assert!(md.contains("| G9 correctness |"), "{md}");
    }

    /// Each gate's miss fails `--gate`.
    #[test]
    fn each_gate_can_fail() {
        type Breaker = fn(&mut RepoReport);
        let breakers: [(&str, Breaker); 10] = [
            ("G1", |r| r.creates_refused = 1),
            ("G2", |r| {
                r.samples[4].free_bytes = vec![10 * GIB];
                r.samples[3].free_bytes = vec![10 * GIB];
            }),
            ("G3", |r| r.samples[4].free_bytes = vec![125 * GIB]),
            ("G4", |r| r.samples[5].free_bytes = vec![140 * GIB]),
            ("G5", |r| r.samples[4].load1 = 30.0),
            ("G6", |r| r.samples[4].pressure_level = Some(4)),
            ("G7", |r| r.list_ms.push((3500, 5000))),
            ("G7", |r| r.list_failures.push(3600)),
            ("G8", |r| {
                for t in &mut r.five {
                    t.wall_ms = 6000;
                }
            }),
            ("G9", |r| {
                r.five[0]
                    .outcomes
                    .insert("cargo:c1:t".to_owned(), Status::Fail);
            }),
        ];
        for (id, brk) in breakers {
            let mut repo = passing();
            brk(&mut repo);
            repo.summary = summarize(&repo, 8);
            let report = report(Profile::Standard, repo);
            let failed: Vec<_> = gates(&report.repos[0], report.profile, "macos")
                .into_iter()
                .filter(|g| g.verdict == Verdict::Fail)
                .map(|g| g.id)
                .collect();
            assert!(failed.contains(&id), "{id}: failed {failed:?}");
            assert!(any_failed(&report), "{id}");
        }
    }

    #[test]
    fn dev_runs_and_reported_rows_are_never_judged() {
        let mut repo = passing();
        repo.creates_refused = 3;
        repo.summary = summarize(&repo, 8);
        let dev = report(Profile::Dev, repo.clone());
        assert!(
            gates(&dev.repos[0], dev.profile, "macos")
                .iter()
                .all(|g| g.verdict == Verdict::Dev)
        );
        assert!(!any_failed(&dev));
        repo.gated = false;
        let reported = report(Profile::Standard, repo);
        assert!(
            gates(&reported.repos[0], reported.profile, "macos")
                .iter()
                .all(|g| g.verdict == Verdict::Reported)
        );
        assert!(!any_failed(&reported));
    }

    #[test]
    fn rust_gets_three_times_and_linux_reads_psi() {
        assert!((g3_limit(Some(Stack::Rust)) - 3.0).abs() < f64::EPSILON);
        assert!((g3_limit(Some(Stack::Python)) - 2.0).abs() < f64::EPSILON);
        let mut repo = passing();
        for s in &mut repo.samples {
            s.psi_full_avg10 = Some(12.0);
        }
        repo.summary = summarize(&repo, 8);
        let g6 = gates(&repo, Profile::Linux, "linux")
            .into_iter()
            .find(|g| g.id == "G6")
            .unwrap();
        assert_eq!(g6.verdict, Verdict::Fail);
    }

    #[test]
    fn an_error_turns_passes_into_not_measured_and_fails_the_gate() {
        let mut repo = passing();
        repo.error = Some("clone failed".to_owned());
        let report = report(Profile::Standard, repo);
        assert!(
            gates(&report.repos[0], report.profile, "macos")
                .iter()
                .all(|g| g.verdict == Verdict::NotMeasured)
        );
        assert!(any_failed(&report));
    }

    #[test]
    fn the_report_round_trips() {
        let report = report(Profile::Linux, passing());
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(serde_json::from_str::<Report>(&json).unwrap(), report);
    }
}
