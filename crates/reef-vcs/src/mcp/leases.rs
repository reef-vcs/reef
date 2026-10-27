//! The leases `reef mcp` keeps alive: every active workspace it created,
//! and every active workspace with its owner that a call named, for as long
//! as the server runs. Nothing is released when it exits: a harness that
//! restarts its server must not lose a directory its agent is editing, and
//! a lapsed lease is reclaimed with a final snapshot (ADR 0019, ADR 0035).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use reef_api::v1::{Workspace, WorkspaceState};
use tokio::task::JoinHandle;

use crate::cli::Ctx;
use crate::cli::heartbeat::{Heartbeater, heartbeats};

/// One heartbeat task per leased workspace, by workspace id.
pub(super) struct Leases {
    /// `<user>@<host>:mcp`: the owner of the workspaces the server creates,
    /// the same for every server of this user here, so a restarted one
    /// takes over its predecessor's.
    pub(super) owner: String,
    tasks: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl Leases {
    pub(super) fn new(owner: String) -> Self {
        Self {
            owner,
            tasks: Mutex::new(HashMap::new()),
        }
    }

    /// Starts heartbeating `ws` if it is active, has the server's owner, and
    /// is not heartbeated already.
    pub(super) fn keep(&self, ctx: &Ctx, ws: &Workspace) {
        if ws.state != i32::from(WorkspaceState::Active) || ws.owner != self.owner {
            return;
        }
        let Some(lease) = &ws.lease else {
            return;
        };
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        if tasks.get(&ws.id).is_some_and(|task| !task.is_finished()) {
            return;
        }
        let notify = Box::new(|text: &str| tracing::warn!(target: "reef", "{text}"));
        match Heartbeater::new(Arc::new(ctx.client.clone()), ws, lease, notify) {
            Ok(heartbeater) => {
                tasks.insert(ws.id.clone(), tokio::spawn(heartbeats(heartbeater)));
            }
            Err(error) => {
                tracing::warn!(target: "reef", workspace = %ws.id, %error, "cannot keep the lease alive");
            }
        }
    }

    /// Stops heartbeating workspace `id`, after its release.
    pub(super) fn end(&self, id: &str) {
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(task) = tasks.remove(id) {
            task.abort();
        }
    }
}

impl Drop for Leases {
    fn drop(&mut self) {
        let tasks = self.tasks.get_mut().unwrap_or_else(PoisonError::into_inner);
        for task in tasks.values() {
            task.abort();
        }
    }
}
