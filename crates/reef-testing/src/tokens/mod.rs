//! The token-efficiency benchmark (R2.17): what it costs an agent that
//! knows Reef to answer standard questions about a repository where several
//! agents work, with git porcelain, the Reef CLI, and the Reef MCP server.
//!
//! One fixture ([`fixture`]) is built twice from Reef's own history, with
//! identical commits: unfinished work sits in git worktrees in one copy and
//! in Reef workspaces in the other. Six questions ([`scenario`]) are asked
//! of it in two ways:
//!
//! - **Scripted** ([`scripted`]): fixed command sequences or MCP calls
//!   ([`mcp`]) per question and arm, counted ([`count`]) in bytes,
//!   Anthropic's token counting, and the published `o200k_base`; the cheapest
//!   sequence whose output holds the answer is the arm's number.
//! - **Live** ([`live`]): real agents through [`crate::agent`], three trials
//!   per question and arm, graded ([`grade`]) against the truth.
//!
//! [`report`] turns results into the tables of `docs/token-benchmark.md`
//! and the Phase 2 exit criterion's verdict.

use serde::{Deserialize, Serialize};

pub mod count;
pub mod fixture;
pub mod grade;
pub mod live;
pub mod mcp;
pub mod report;
pub mod scenario;
pub mod scripted;

/// How the agent answers.
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
#[serde(rename_all = "snake_case")]
pub enum Arm {
    /// A shell in a git worktree; no `reef`.
    Git,
    /// A shell in a Reef workspace, `reef` on `PATH`.
    Reef,
    /// Only the Reef MCP server's tools.
    Mcp,
}

impl Arm {
    /// Every arm, in order.
    pub const ALL: [Arm; 3] = [Arm::Git, Arm::Reef, Arm::Mcp];

    /// The arm's name in files and tables.
    pub fn name(self) -> &'static str {
        match self {
            Arm::Git => "git",
            Arm::Reef => "reef",
            Arm::Mcp => "mcp",
        }
    }
}

impl std::fmt::Display for Arm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The version of the result files this build writes.
pub const SCHEMA: u32 = 1;
