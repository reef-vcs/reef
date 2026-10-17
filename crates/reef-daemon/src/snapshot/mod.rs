//! Snapshots of workspaces. R1.05 defines only why a snapshot is taken;
//! R1.13 adds the watcher-driven scheduler here and R1.16 stores the kind in
//! `snapshots`.

/// Why a snapshot was taken, as R1.16's `snapshots.kind` records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotKind {
    /// The filesystem watcher saw changes (R1.13).
    Watcher,
    /// A client asked: the `Snapshot` RPC, or `Inspect` with
    /// `snapshot_first`.
    Explicit,
    /// Before a release or reclaim.
    Final,
    /// Before a workspace moves to another change (R1.06).
    Move,
    /// While the daemon shuts down.
    Shutdown,
}

impl SnapshotKind {
    /// The stored name: `watcher`, `explicit`, `final`, `move`, `shutdown`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Watcher => "watcher",
            Self::Explicit => "explicit",
            Self::Final => "final",
            Self::Move => "move",
            Self::Shutdown => "shutdown",
        }
    }
}
