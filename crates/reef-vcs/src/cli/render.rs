//! Text forms of the [`reef_daemon::client::json`] types, so each command's
//! `run` stays a call and an [`Out`](super::Out) method.

use std::io::{self, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reef_daemon::client::json::{Created, Inspected, Lease, Repository, Status, Workspace};

use super::out::{TextRender, TextRow, block};

/// Owners and other free text are cut to this many characters in tables.
const NAME_WIDTH: usize = 40;
/// Change ids are shown by this many characters of their reverse hex.
const CHANGE_WIDTH: usize = 8;

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

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// A span as the largest whole unit: `45s`, `12m`, `3h`, `2d`.
fn span(ms: i64) -> String {
    let secs = ms.unsigned_abs() / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// `12m left`, `expired 3m ago`, or `-` without a lease.
fn lease_text(lease: Option<&Lease>, now: i64) -> String {
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
        ("watcher", ws.watcher.clone()),
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
    ]
}

/// Only the path, so `cd "$(reef ws create)"` works.
impl TextRender for Created {
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
        block(w, &pairs)
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

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        assert_eq!(lease_text(Some(&lease(1_000 + 720_000)), 1_000), "12m left");
        assert_eq!(lease_text(Some(&lease(1_000)), 181_000), "expired 3m ago");
        assert_eq!(lease_text(None, 0), "-");
    }

    #[test]
    fn long_names_are_cut() {
        assert_eq!(truncate("short", 40), "short");
        let long = "x".repeat(50);
        assert_eq!(truncate(&long, 40).len(), 40);
        assert!(truncate(&long, 40).ends_with("..."));
    }
}
