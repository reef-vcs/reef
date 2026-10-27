//! What `reef mcp` reports as presence (R2.11): one `ACTIVITY` entry per
//! repository or workspace its calls name, saying the last tool called
//! there. Calls come in bursts, so the entries are never refreshed: each
//! call replaces its subject's entry with a TTL of a minute, and an entry
//! that lapses says the agent went quiet. Entries still held when the
//! server exits are cleared.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use reef_api::v1::PresenceKind;
use reef_client::presence::{self, CLEAR_DEADLINE, Cadence, Reporter, State, Subject};
use tokio::task::JoinSet;

use crate::cli::Ctx;
use crate::cli::scope::Scope;

/// One reporter per subject, by repository and workspace id (empty in
/// repository scope).
pub(super) struct Activity {
    /// The agent when neither `REEF_AGENT` nor a workspace's owner names
    /// one: the server's owner name.
    fallback: String,
    reporters: Mutex<Reporters>,
}

#[derive(Default)]
struct Reporters {
    by_subject: HashMap<(String, String), Reporter>,
    /// Set by [`Activity::clear_all`]: a call still being served while the
    /// server stops reports nothing that would outlive it.
    closed: bool,
}

impl Activity {
    pub(super) fn new(fallback: String) -> Self {
        Self {
            fallback,
            reporters: Mutex::new(Reporters::default()),
        }
    }

    /// Reports that `tool` was called in `scope`. Reporters whose entries
    /// lapsed are dropped first, so the map holds the subjects of the last
    /// minute.
    pub(super) fn called(&self, ctx: &Ctx, tool: &str, scope: &Scope) {
        let ws = scope.workspace.as_ref();
        let key = (scope.repo_id.clone(), scope.workspace_id());
        let mut reporters = self
            .reporters
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if reporters.closed {
            return;
        }
        reporters
            .by_subject
            .retain(|_, reporter| reporter.is_live());
        let reporter = reporters.by_subject.entry(key).or_insert_with(|| {
            let subject = Subject {
                agent: presence::agent(
                    ctx.agent.as_deref(),
                    ws.map(|ws| ws.owner.as_str()),
                    &self.fallback,
                ),
                repo: if ws.is_some() {
                    String::new()
                } else {
                    scope.repo_id.clone()
                },
                workspace: scope.workspace_id(),
                kind: PresenceKind::Activity,
            };
            Reporter::start(Arc::new(ctx.client.clone()), subject, Cadence::BURST)
        });
        reporter.set(State {
            change: ws.map(|ws| ws.change_id.clone()).unwrap_or_default(),
            activity: tool.to_owned(),
            ..State::default()
        });
    }

    /// Clears every entry held, together, within [`CLEAR_DEADLINE`], and
    /// reports nothing more.
    pub(super) async fn clear_all(&self) {
        let reporters: Vec<Reporter> = {
            let mut reporters = self
                .reporters
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            reporters.closed = true;
            reporters
                .by_subject
                .drain()
                .map(|(_, reporter)| reporter)
                .collect()
        };
        let mut clears = JoinSet::new();
        for reporter in reporters {
            clears.spawn(reporter.clear());
        }
        let _ = tokio::time::timeout(CLEAR_DEADLINE, clears.join_all()).await;
    }
}
