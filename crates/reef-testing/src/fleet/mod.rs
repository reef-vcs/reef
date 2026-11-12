//! The fleet benchmark (#264): what five tickets at once cost a standard
//! laptop, in disk, load, memory and responsiveness.
//!
//! For each pinned repository ([`repos`]) the harness runs one scenario
//! ([`run`]) through the `reef` CLI, as an agent would, against a daemon
//! with its own state directory:
//!
//! 1. **Solo**: one `reef ws run` per ticket, one at a time, each applying
//!    its patch and running the build and tests; ticket 1's is the "one
//!    workspace" every ratio divides by, and each one's test outcomes are
//!    the reference for correctness.
//! 2. **Five at once**: five `reef ws run --keep`, started within a second,
//!    each with its patch; then each workspace's second, smaller patch (the
//!    review round) in the same workspace; then idle, then released.
//! 3. **After**: `reef ws gc` and the final free space.
//!
//! [`sample`] reads free space, load, memory pressure and the tickets'
//! memory every second, and a canary measures how responsive the machine
//! stays. [`report`] writes `report.json` and `summary.md` and judges the
//! gates. [`seed`] is the build-output spike's way of starting a workspace
//! with another one's build directories. See `docs/fleet-benchmark.md`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub mod disk;
pub mod outcomes;
pub mod report;
pub mod repos;
pub mod run;
pub mod sample;
pub mod seed;

/// The machine a run stands for. Only `standard` and `linux` runs decide a
/// gate; `dev` runs are for iterating and are labelled apart.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// The standard machine's limits: a macOS VM with 8 vCPUs, 16 GiB and a
    /// 512 GB disk.
    Standard,
    /// The `ubuntu-24.04` hosted runner: 4 vCPUs, 16 GiB.
    Linux,
    /// Any other host, such as the dev machine. Never judged.
    Dev,
}

impl Profile {
    /// Whether gates are judged on this profile's runs.
    pub fn judged(self) -> bool {
        !matches!(self, Profile::Dev)
    }
}

impl std::fmt::Display for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Profile::Standard => "standard",
            Profile::Linux => "linux",
            Profile::Dev => "dev",
        })
    }
}

/// On or off, for a variant's flags.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Toggle {
    /// Off.
    Off,
    /// On.
    On,
}

impl std::fmt::Display for Toggle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Toggle::Off => "off",
            Toggle::On => "on",
        })
    }
}

/// How a ticket's workspace starts, for the build-output spike (#264,
/// experiments E1 to E3). Seeding clones a source workspace's build
/// directories into the new workspace before the patch is applied.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "kebab-case")]
pub enum SeedMode {
    /// Every workspace starts with no build output (the baseline).
    Off,
    /// From a sibling built on the same base, unedited (E1).
    Sibling,
    /// From a sibling that applied ticket 1's patch before building (E2).
    SiblingEdited,
    /// From a sibling's build reduced to dependencies only by the
    /// repository's `seed_prepare` (E3, the shape of a clone store).
    Clone,
}

impl std::fmt::Display for SeedMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SeedMode::Off => "off",
            SeedMode::Sibling => "sibling",
            SeedMode::SiblingEdited => "sibling-edited",
            SeedMode::Clone => "clone",
        })
    }
}

/// What a run varied, so reports compare like with like.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variant {
    /// The machine-wide build limit (#271). Reef has none yet, so only
    /// `off` runs.
    pub limit: Toggle,
    /// How workspaces start.
    pub seed: SeedMode,
    /// Whether the repository's `reef.toml` stores are declared.
    pub stores: Toggle,
}

impl std::fmt::Display for Variant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "limit {}, seed {}, stores {}",
            self.limit, self.seed, self.stores
        )
    }
}

/// The machine a run measured on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Host {
    /// `macos` or `linux`.
    pub os: String,
    /// `aarch64`, `x86_64`.
    pub arch: String,
    /// Logical cores.
    pub cores: u32,
    /// Performance cores, where the OS tells (Apple silicon).
    pub performance_cores: Option<u32>,
    /// Physical memory in bytes.
    pub memory_bytes: u64,
    /// The volume holding the workspaces: total bytes.
    pub volume_bytes: u64,
    /// `reef --version`.
    pub reef_version: String,
    /// Tool versions (`cargo`, `go`, `node`, `uv`), as each prints them.
    pub tools: BTreeMap<String, String>,
    /// Time Machine's local snapshots on the root volume (macOS): a released
    /// workspace's blocks stay allocated while one holds them.
    pub local_snapshots: Vec<String>,
    /// Spotlight's indexing state for the data volume (macOS), as
    /// `mdutil -s` prints it.
    pub spotlight: Option<String>,
}

/// One timed `reef ws run`: a ticket's round in a workspace.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TicketRun {
    /// 1-based ticket number; its patch is `<ticket>.patch`.
    pub ticket: u32,
    /// 1 for the ticket's patch, 2 for its review round (`<ticket>b.patch`).
    pub round: u32,
    /// The workspace, from `ws run`'s report.
    pub workspace_id: Option<String>,
    /// The command's exit code, as `ws run` reports it; `None` when `ws
    /// run` itself failed.
    pub exit_code: Option<i32>,
    /// From the harness starting `reef ws run` to its exit: create (or
    /// attach), the patch, the build and tests, and the snapshot or release.
    pub wall_ms: u64,
    /// When it started, in milliseconds since the repository's scenario
    /// started.
    pub started_ms: u64,
    /// Each test's result, read from the log.
    pub outcomes: BTreeMap<String, outcomes::Status>,
    /// Compilation units the build compiled (cargo's `Compiling` lines),
    /// for the spike's freshness question.
    pub units_compiled: Option<u32>,
    /// The peak summed RSS of `ws run`'s process tree, in bytes.
    pub peak_rss_bytes: u64,
    /// The workspace's disk after this round, if it was kept.
    pub disk: Option<disk::WorkspaceDisk>,
    /// Whether the log says a write failed for disk (`ENOSPC`).
    pub enospc: bool,
    /// Why `ws run` failed, when it did (a refused create, for example).
    pub error: Option<String>,
    /// The log, relative to the report.
    pub log: String,
}

impl TicketRun {
    /// Whether the build and tests passed.
    pub fn passed(&self) -> bool {
        self.exit_code == Some(0)
    }
}
