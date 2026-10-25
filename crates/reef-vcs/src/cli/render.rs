//! Text forms of the [`reef_api::json`] types, so each command's
//! `run` stays a call and an [`Out`](super::Out) method.

use std::io::{self, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reef_api::json::{
    Created, GcReport, Inspected, Lease, Moved, Repository, Status, Store, Workspace,
};

use serde::Serialize;

use super::out::{TextRender, TextRow, block};

/// Owners and other free text are cut to this many characters in tables.
const NAME_WIDTH: usize = 40;
/// Change ids are shown by this many characters of their reverse hex.
const CHANGE_WIDTH: usize = 8;
/// Operation ids are shown by this many characters, as `jj op log` does.
const OP_WIDTH: usize = 12;

fn or_dash(value: Option<&str>) -> String {
    value.unwrap_or("-").to_owned()
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(width - 3).collect();
    cut.push_str("...");
    cut
}

pub(super) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// A span as the largest whole unit: `45s`, `12m`, `3h`, `2d`.
pub(super) fn span(ms: i64) -> String {
    let secs = ms.unsigned_abs() / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// `12m left`, `expired 3m ago`, `ended (released)`, or `-` without a
/// lease.
pub(super) fn lease_text(lease: Option<&Lease>, now: i64) -> String {
    if let Some(lease) = lease.filter(|lease| lease.state == "ended") {
        return match &lease.end_reason {
            Some(reason) => format!("ended ({reason})"),
            None => "ended".to_owned(),
        };
    }
    match lease.and_then(|lease| lease.expires_at_ms) {
        Some(expires) if expires >= now => format!("{} left", span(expires - now)),
        Some(expires) => format!("expired {} ago", span(now - expires)),
        None => "-".to_owned(),
    }
}

fn short_change(ws: &Workspace) -> String {
    ws.change_id_reverse_hex
        .chars()
        .take(CHANGE_WIDTH)
        .collect()
}

impl TextRow for Repository {
    const HEADER: &'static [&'static str] = &["ID", "ROOT", "COLOCATED", "REGISTERED"];

    fn row(&self) -> Vec<String> {
        vec![
            self.id.clone(),
            self.root.clone(),
            self.colocated.to_string(),
            or_dash(self.registered_at.as_deref()),
        ]
    }
}

impl TextRender for Repository {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        block(
            w,
            &[
                ("id", self.id.clone()),
                ("root", self.root.clone()),
                ("colocated", self.colocated.to_string()),
                ("registered_at", or_dash(self.registered_at.as_deref())),
            ],
        )
    }
}

impl TextRow for Workspace {
    const HEADER: &'static [&'static str] = &[
        "ID",
        "STATE",
        "CHANGE",
        "OWNER",
        "LEASE",
        "BACKEND",
        "ATTENTION",
        "PATH",
    ];

    fn row(&self) -> Vec<String> {
        vec![
            self.id.clone(),
            self.state.clone(),
            short_change(self),
            truncate(&self.owner, NAME_WIDTH),
            lease_text(self.lease.as_ref(), now_ms()),
            self.backend.clone(),
            truncate(&or_dash(self.attention.as_deref()), NAME_WIDTH),
            self.path.clone(),
        ]
    }
}

fn workspace_pairs(ws: &Workspace) -> Vec<(&'static str, String)> {
    vec![
        ("id", ws.id.clone()),
        ("repo", ws.repo_id.clone()),
        ("state", ws.state.clone()),
        ("path", ws.path.clone()),
        (
            "change",
            format!("{} ({})", ws.change_id_reverse_hex, ws.change_id),
        ),
        ("commit", ws.commit_id.clone()),
        ("owner", ws.owner.clone()),
        ("purpose", ws.purpose.clone()),
        ("backend", ws.backend.clone()),
        ("watcher", watcher_text(ws)),
        ("divergent", ws.divergent.to_string()),
        ("attention", or_dash(ws.attention.as_deref())),
        ("created_at", or_dash(ws.created_at.as_deref())),
        ("last_snapshot_at", or_dash(ws.last_snapshot_at.as_deref())),
        ("last_snapshot_op", or_dash(ws.last_snapshot_op.as_deref())),
        ("lease", lease_text(ws.lease.as_ref(), now_ms())),
        (
            "lease_expires_at",
            or_dash(ws.lease.as_ref().and_then(|l| l.expires_at.as_deref())),
        ),
        ("stores", stores_text(ws)),
    ]
}

/// `watchman`, or `watchman (waiting for creates to finish)` while its
/// subscription is deferred (R1.29); `notify (attaching)` while a notify
/// watch attaches in the background (R1.32).
fn watcher_text(ws: &Workspace) -> String {
    match ws.watch_state.as_deref() {
        Some("waiting") => format!("{} (waiting for creates to finish)", ws.watcher),
        Some(state) => format!("{} ({state})", ws.watcher),
        None => ws.watcher.clone(),
    }
}

/// `deps at vendor/deps (symlink), tools at .tools (bind, lockfile
/// changed)`, or `-`.
fn stores_text(ws: &Workspace) -> String {
    if ws.stores.is_empty() {
        return "-".to_owned();
    }
    ws.stores
        .iter()
        .map(|store| {
            let changed = if store.verified {
                ""
            } else {
                ", lockfile changed"
            };
            format!(
                "{} at {} ({}{changed})",
                store.name, store.mount, store.mode
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

impl TextRow for Store {
    const HEADER: &'static [&'static str] =
        &["NAME", "LOCKFILE", "SIZE", "IN_USE", "BUILT", "PATH"];

    fn row(&self) -> Vec<String> {
        vec![
            self.name.clone(),
            self.lockfile_hash.chars().take(OP_WIDTH).collect(),
            size(self.bytes),
            self.in_use_by.to_string(),
            or_dash(self.built_at.as_deref()),
            self.path.clone(),
        ]
    }
}

/// Only the path, so `cd "$(reef ws create)"` works.
impl TextRender for Created {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "{}", self.workspace.path)
    }
}

/// The path, which a move keeps, so `cd "$(reef ws move -o text ...)"` works
/// as after `ws create`.
impl TextRender for Moved {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "{}", self.workspace.path)
    }
}

impl TextRender for Inspected {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let mut pairs = workspace_pairs(&self.workspace);
        pairs.push((
            "disk_usage",
            self.disk_usage.map_or_else(
                || "-".to_owned(),
                |usage| {
                    format!(
                        "{} bytes in {} changed files (estimate)",
                        usage.allocated_bytes, usage.files_changed
                    )
                },
            ),
        ));
        pairs.push((
            "others_on_change",
            if self.other_workspaces_on_change.is_empty() {
                "-".to_owned()
            } else {
                self.other_workspaces_on_change.join(", ")
            },
        ));
        pairs.push((
            "snapshots",
            format!(
                "{} ({} local)",
                self.snapshot_count, self.workspace.local_snapshot_count
            ),
        ));
        block(w, &pairs)?;
        if !self.recent_snapshots.is_empty() {
            writeln!(w, "recent_snapshots:")?;
            for snapshot in &self.recent_snapshots {
                writeln!(
                    w,
                    "  {}  {:<8}  {}  {}",
                    or_dash(snapshot.taken_at.as_deref()),
                    snapshot.kind,
                    if snapshot.local { "local" } else { "published" },
                    snapshot.op_id.chars().take(OP_WIDTH).collect::<String>(),
                )?;
            }
        }
        Ok(())
    }
}

impl TextRender for Status {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let backends = self
            .backends
            .iter()
            .map(|probe| match &probe.reason {
                _ if probe.available => format!("{} ({})", probe.kind, probe.filesystem),
                Some(reason) => format!("{} (unavailable: {reason})", probe.kind),
                None => format!("{} (unavailable)", probe.kind),
            })
            .collect::<Vec<_>>()
            .join(", ");
        block(
            w,
            &[
                ("version", self.version.clone()),
                ("pid", self.pid.to_string()),
                (
                    "uptime",
                    humantime::format_duration(Duration::from_secs(self.uptime_ms / 1000))
                        .to_string(),
                ),
                ("socket", self.socket.clone()),
                ("state_dir", self.state_dir.clone()),
                ("schema_version", self.schema_version.to_string()),
                ("repositories", self.repositories.to_string()),
                ("active_workspaces", self.active_workspaces.to_string()),
                ("backends", backends),
            ],
        )
    }
}

/// A byte count in binary units, as `du -h` would: `0 B`, `512 B`,
/// `1.2 GiB`, `41 MiB` (one decimal below ten).
#[allow(
    clippy::cast_precision_loss,
    reason = "a rounded display; f64 is exact far past any disk"
)]
pub(crate) fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// One `ws gc` line after its label: `jj 120 MiB -> 41 MiB, bases 3
/// removed (1.2 GiB), workspaces 7 rows (18 MiB), stores 0`.
fn gc_line(report: &GcReport) -> String {
    let stores = if report.stores_removed == 0 {
        "0".to_owned()
    } else {
        format!(
            "{} removed ({})",
            report.stores_removed,
            size(report.store_bytes_freed)
        )
    };
    format!(
        "jj {} -> {}, bases {} removed ({}), workspaces {} rows ({}), stores {stores}",
        size(report.jj_bytes_before),
        size(report.jj_bytes_after),
        report.base_entries_removed,
        size(report.base_bytes_freed),
        report.workspace_rows_removed,
        size(report.wc_bytes_freed),
    )
}

/// `ws gc`'s result: the reports, a JSON array; in text, one line per
/// repository, then the total, each saying so in a dry run.
#[derive(Serialize)]
#[serde(transparent)]
pub(crate) struct GcReports {
    pub(crate) reports: Vec<GcReport>,
    #[serde(skip)]
    pub(crate) dry_run: bool,
}

impl TextRender for GcReports {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let dry = if self.dry_run { " (dry run)" } else { "" };
        let mut total = GcReport {
            repo_id: String::new(),
            keep_newer: None,
            jj_bytes_before: 0,
            jj_bytes_after: 0,
            reindex_ms: 0,
            jj_error: None,
            base_entries_removed: 0,
            base_bytes_freed: 0,
            workspace_rows_removed: 0,
            wc_bytes_freed: 0,
            stores_removed: 0,
            store_bytes_freed: 0,
            elapsed_ms: 0,
            dry_run: self.dry_run,
        };
        for report in &self.reports {
            writeln!(w, "repo {}{dry}: {}", report.repo_id, gc_line(report))?;
            total.jj_bytes_before += report.jj_bytes_before;
            total.jj_bytes_after += report.jj_bytes_after;
            total.base_entries_removed += report.base_entries_removed;
            total.base_bytes_freed += report.base_bytes_freed;
            total.workspace_rows_removed += report.workspace_rows_removed;
            total.wc_bytes_freed += report.wc_bytes_freed;
            total.stores_removed += report.stores_removed;
            total.store_bytes_freed += report.store_bytes_freed;
        }
        writeln!(w, "total{dry}: {}", gc_line(&total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(512), "512 B");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(41 << 20), "41 MiB");
        assert_eq!(size((12 << 30) / 10), "1.2 GiB");
    }

    #[test]
    fn gc_reports_read_as_one_line_each_and_a_total() {
        let report = |id: &str, before: u64, after: u64| GcReport {
            repo_id: id.to_owned(),
            keep_newer: None,
            jj_bytes_before: before,
            jj_bytes_after: after,
            reindex_ms: 5,
            jj_error: None,
            base_entries_removed: 3,
            base_bytes_freed: (12 << 30) / 10,
            workspace_rows_removed: 7,
            wc_bytes_freed: 18 << 20,
            stores_removed: 0,
            store_bytes_freed: 0,
            elapsed_ms: 9,
            dry_run: false,
        };
        let reports = GcReports {
            reports: vec![report("aaaa", 120 << 20, 41 << 20), report("bbbb", 0, 0)],
            dry_run: false,
        };
        let mut buffer = Vec::new();
        reports.render(&mut buffer).unwrap();
        assert_eq!(
            String::from_utf8(buffer).unwrap(),
            "repo aaaa: jj 120 MiB -> 41 MiB, bases 3 removed (1.2 GiB), workspaces 7 rows (18 MiB), stores 0\n\
             repo bbbb: jj 0 B -> 0 B, bases 3 removed (1.2 GiB), workspaces 7 rows (18 MiB), stores 0\n\
             total: jj 120 MiB -> 41 MiB, bases 6 removed (2.4 GiB), workspaces 14 rows (36 MiB), stores 0\n"
        );
        let json = serde_json::to_value(&reports).unwrap();
        assert!(json.is_array(), "{json}");
    }

    #[test]
    fn spans_use_the_largest_unit() {
        assert_eq!(span(45_000), "45s");
        assert_eq!(span(12 * 60_000 + 5), "12m");
        assert_eq!(span(3 * 3_600_000), "3h");
        assert_eq!(span(-2 * 86_400_000), "2d");
    }

    #[test]
    fn leases_read_as_time_left_or_ago() {
        let lease = |expires: i64| Lease {
            id: String::new(),
            owner: String::new(),
            issued_at: None,
            expires_at: None,
            expires_at_ms: Some(expires),
            last_heartbeat_at: None,
            length_ms: 0,
            grace_ms: 0,
            state: "active".to_owned(),
            end_reason: None,
            heartbeat_every_ms: None,
        };
        assert_eq!(lease_text(Some(&lease(1_000 + 720_000)), 1_000), "12m left");
        assert_eq!(lease_text(Some(&lease(1_000)), 181_000), "expired 3m ago");
        assert_eq!(lease_text(None, 0), "-");
        let ended = Lease {
            state: "ended".to_owned(),
            end_reason: Some("expired".to_owned()),
            ..lease(1_000)
        };
        assert_eq!(lease_text(Some(&ended), 181_000), "ended (expired)");
    }

    #[test]
    fn long_names_are_cut() {
        assert_eq!(truncate("short", 40), "short");
        let long = "x".repeat(50);
        assert_eq!(truncate(&long, 40).len(), 40);
        assert!(truncate(&long, 40).ends_with("..."));
    }
}
