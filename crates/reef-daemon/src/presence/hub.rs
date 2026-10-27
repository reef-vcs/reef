//! The live presence set.
//!
//! One `std` mutex guards the entries; it is held only to change them and
//! to send on the broadcast channel, which never blocks, so broadcast order
//! is change order and a subscription can take the set and a receiver
//! atomically. Every state an entry takes gets a hub-wide `seq`, which is
//! how a session tells a newer state from one it already has.
//!
//! An entry's changes are broadcast at most once per
//! [`Limits::publish_interval`]: the first after a quiet interval at once,
//! later ones folded into one broadcast of the latest state at the
//! interval's end. Ends are broadcast at once. One timer task runs the
//! expiries and those deferred broadcasts, sleeping until the earliest.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use prost::Message as _;
use reef_api::v1::{self, PresenceEndReason, PresenceKind};
use tokio::sync::{Notify, broadcast};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::filter::PresenceFilter;
use super::{ENDED_RING, ENDED_WORKSPACES, Limits, MAX_ENTRY_BYTES, PresenceError};
use crate::model::{PresenceId, RepoId, Timestamp, WorkspaceId};

/// An entry the daemon reports itself, keyed so its publisher replaces it
/// without holding its id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DaemonKey {
    /// A workspace's lease heartbeats.
    Lease(WorkspaceId),
    /// The files a workspace's snapshots change.
    Editing(WorkspaceId),
    /// One long-running operation's progress.
    Progress(u64),
}

/// What an entry says, as a report gives it: validated and resolved.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Who reports it.
    pub agent: String,
    /// The repository it is about.
    pub repo: Option<RepoId>,
    /// The workspace it is about.
    pub workspace: Option<WorkspaceId>,
    /// The change it is about, as full hex; empty for none.
    pub change_id: String,
    /// What it says.
    pub kind: PresenceKind,
    /// For `ACTIVITY`.
    pub activity: String,
    /// For `EDITING`.
    pub paths: Vec<String>,
    /// For `PROGRESS`.
    pub progress: Option<v1::Progress>,
    /// For `LEASE`.
    pub lease: Option<v1::Lease>,
    /// How long it lasts unless reported again, already clamped.
    pub ttl: Duration,
}

/// One state of one entry, as broadcast and as a subscription's set holds
/// it.
#[derive(Clone, Debug)]
pub struct Change {
    /// The entry.
    pub id: PresenceId,
    /// When this state was taken, hub-wide; larger is newer.
    pub seq: u64,
    /// The entry's repository, for filters.
    pub repo: Option<RepoId>,
    /// The entry's workspace, for filters.
    pub workspace: Option<WorkspaceId>,
    /// The entry as of this state; its last state when ended.
    pub presence: v1::Presence,
    /// Why it ended; `None` while it lives.
    pub end: Option<PresenceEndReason>,
}

impl Change {
    /// The change as a wire event.
    pub fn event(&self) -> v1::PresenceEvent {
        v1::PresenceEvent {
            presence: Some(self.presence.clone()),
            ended: self.end.is_some(),
            end_reason: self.end.unwrap_or(PresenceEndReason::Unspecified).into(),
        }
    }

    /// This entry ended for `reason`, at its last state.
    pub(crate) fn ended(&self, seq: u64, reason: PresenceEndReason) -> Change {
        Change {
            seq,
            end: Some(reason),
            ..self.clone()
        }
    }
}

/// Something the timer task does at an instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Due {
    /// The entry lapses.
    Expire,
    /// The entry's deferred broadcast goes out.
    Publish,
}

#[derive(Debug)]
struct Entry {
    /// The current state.
    change: Arc<Change>,
    expires: Instant,
    daemon: Option<DaemonKey>,
    /// When its last broadcast went out.
    broadcast_at: Option<Instant>,
    /// A broadcast of its latest state is deferred to this instant.
    deferred: Option<Instant>,
}

#[derive(Debug, Default)]
struct Inner {
    seq: u64,
    entries: BTreeMap<PresenceId, Entry>,
    daemon_keys: HashMap<DaemonKey, PresenceId>,
    per_agent: HashMap<String, usize>,
    clients: usize,
    deadlines: BTreeSet<(Instant, PresenceId, Due)>,
    ended: VecDeque<PresenceId>,
    ended_reasons: HashMap<PresenceId, PresenceEndReason>,
    ended_workspaces: VecDeque<WorkspaceId>,
    ended_workspace_set: HashSet<WorkspaceId>,
    ended_repos: VecDeque<RepoId>,
    ended_repo_set: HashSet<RepoId>,
}

impl Inner {
    /// Whether `report` names a workspace that ended or a repository that
    /// was forgotten: a report that raced the end.
    fn names_gone(&self, report: &Report) -> bool {
        report
            .workspace
            .is_some_and(|workspace| self.ended_workspace_set.contains(&workspace))
            || report
                .repo
                .is_some_and(|repo| self.ended_repo_set.contains(&repo))
    }
}

/// Remembers `id` in a ring of `capacity`, forgetting the oldest.
fn remember<T: Copy + Eq + std::hash::Hash>(
    ring: &mut VecDeque<T>,
    set: &mut HashSet<T>,
    id: T,
    capacity: usize,
) {
    if set.insert(id) {
        ring.push_back(id);
        if ring.len() > capacity
            && let Some(old) = ring.pop_front()
        {
            set.remove(&old);
        }
    }
}

/// The live presence entries; one in [`crate::AppState::presence`].
#[derive(Debug)]
pub struct PresenceHub {
    limits: Limits,
    inner: Mutex<Inner>,
    changes: broadcast::Sender<Arc<Change>>,
    /// Wakes the timer task when an earlier deadline is inserted.
    wake: Notify,
}

impl PresenceHub {
    /// An empty hub. Nothing expires until [`PresenceHub::start`].
    pub fn new(limits: Limits) -> PresenceHub {
        let (changes, _) = broadcast::channel(limits.backlog.max(1));
        PresenceHub {
            limits,
            inner: Mutex::new(Inner::default()),
            changes,
            wake: Notify::new(),
        }
    }

    /// The limits it runs with.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts the timer task, which runs expiries and deferred broadcasts
    /// until `shutdown`.
    pub fn start(self: &Arc<Self>, shutdown: CancellationToken) -> JoinHandle<()> {
        let hub = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let next = hub.run_due(Instant::now());
                let sleep = async {
                    match next {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => break,
                    () = hub.wake.notified() => {}
                    () = sleep => {}
                }
            }
        })
    }

    /// Runs everything due at or before `now`; returns the next deadline.
    fn run_due(&self, now: Instant) -> Option<Instant> {
        let mut inner = self.lock();
        while let Some(&(at, id, due)) = inner.deadlines.first() {
            if at > now {
                return Some(at);
            }
            inner.deadlines.pop_first();
            match due {
                Due::Expire => {
                    self.end(&mut inner, id, PresenceEndReason::Expired);
                }
                Due::Publish => {
                    if let Some(entry) = inner.entries.get_mut(&id) {
                        entry.deferred = None;
                        entry.broadcast_at = Some(now);
                        let change = Arc::clone(&entry.change);
                        let _ = self.changes.send(change);
                    }
                }
            }
        }
        None
    }

    /// Reports a new client entry, or replaces the live client entry
    /// `replace` names; returns it as stored.
    pub fn report(
        &self,
        replace: Option<PresenceId>,
        report: Report,
    ) -> Result<v1::Presence, PresenceError> {
        if report.kind == PresenceKind::Lease {
            return Err(PresenceError::DaemonKind);
        }
        let mut inner = self.lock();
        if inner.names_gone(&report) {
            return Err(PresenceError::Ended);
        }
        if let Some(id) = replace {
            let Some(entry) = inner.entries.get(&id) else {
                return Err(PresenceError::NotFound(id));
            };
            if entry.daemon.is_some() {
                return Err(PresenceError::DaemonEntry(id));
            }
            let old = &entry.change;
            let field = if old.presence.kind != i32::from(report.kind) {
                Some("kind")
            } else if old.presence.agent != report.agent {
                Some("agent")
            } else if old.repo != report.repo {
                Some("repo")
            } else if old.workspace != report.workspace {
                Some("workspace")
            } else {
                None
            };
            if let Some(field) = field {
                return Err(PresenceError::Mismatch { id, field });
            }
            return self.store(&mut inner, id, None, report);
        }
        if inner.clients >= self.limits.max_entries {
            return Err(PresenceError::TooMany {
                limit: self.limits.max_entries,
                agent: None,
            });
        }
        let held = inner.per_agent.get(&report.agent).copied().unwrap_or(0);
        if held >= self.limits.max_entries_per_agent {
            return Err(PresenceError::TooMany {
                limit: self.limits.max_entries_per_agent,
                agent: Some(report.agent),
            });
        }
        let id = new_id(&inner);
        let agent = report.agent.clone();
        let presence = self.store(&mut inner, id, None, report)?;
        inner.clients += 1;
        *inner.per_agent.entry(agent).or_default() += 1;
        Ok(presence)
    }

    /// Ends the live client entry `id`; an id that names no live entry is
    /// already clear.
    pub fn clear(&self, id: PresenceId) -> Result<(), PresenceError> {
        let mut inner = self.lock();
        match inner.entries.get(&id) {
            None => Ok(()),
            Some(entry) if entry.daemon.is_some() => Err(PresenceError::DaemonEntry(id)),
            Some(_) => {
                self.end(&mut inner, id, PresenceEndReason::Cleared);
                Ok(())
            }
        }
    }

    /// Publishes or replaces the daemon's entry `key`, built from its
    /// previous state when there is one. Never refused: daemon entries are
    /// not counted against the caps. Returns its id.
    pub fn publish_daemon(
        &self,
        key: DaemonKey,
        build: impl FnOnce(Option<&v1::Presence>) -> Report,
    ) -> PresenceId {
        let mut inner = self.lock();
        let existing = inner.daemon_keys.get(&key).copied();
        let previous = existing
            .and_then(|id| inner.entries.get(&id))
            .map(|entry| &entry.change.presence);
        let report = build(previous);
        if inner.names_gone(&report) {
            return existing.unwrap_or_else(|| new_id(&inner));
        }
        let id = existing.unwrap_or_else(|| new_id(&inner));
        // An entry over the size bound is the daemon's own bug; store it
        // anyway rather than lose the signal.
        let _ = self.store(&mut inner, id, Some(key), report);
        inner.daemon_keys.insert(key, id);
        id
    }

    /// Ends the daemon's entry `key`, if live.
    pub fn end_daemon(&self, key: DaemonKey, reason: PresenceEndReason) {
        let mut inner = self.lock();
        if let Some(id) = inner.daemon_keys.get(&key).copied() {
            self.end(&mut inner, id, reason);
        }
    }

    /// Ends every entry naming `workspace` (`GONE`), and refuses reports
    /// naming it from now on.
    pub fn end_workspace(&self, workspace: WorkspaceId) {
        let mut inner = self.lock();
        let Inner {
            ended_workspaces,
            ended_workspace_set,
            ..
        } = &mut *inner;
        remember(
            ended_workspaces,
            ended_workspace_set,
            workspace,
            ENDED_WORKSPACES,
        );
        let ids: Vec<PresenceId> = inner
            .entries
            .iter()
            .filter(|(_, entry)| entry.change.workspace == Some(workspace))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.end(&mut inner, id, PresenceEndReason::Gone);
        }
    }

    /// Ends every entry naming `repo` (`GONE`), and refuses reports naming
    /// it from now on.
    pub fn end_repository(&self, repo: RepoId) {
        let mut inner = self.lock();
        let Inner {
            ended_repos,
            ended_repo_set,
            ..
        } = &mut *inner;
        remember(ended_repos, ended_repo_set, repo, ENDED_WORKSPACES);
        let ids: Vec<PresenceId> = inner
            .entries
            .iter()
            .filter(|(_, entry)| entry.change.repo == Some(repo))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.end(&mut inner, id, PresenceEndReason::Gone);
        }
    }

    /// The live entries `filter` matches, in id order, and a receiver of
    /// every change after them, taken together.
    pub fn subscribe(
        &self,
        filter: &PresenceFilter,
    ) -> (Vec<Arc<Change>>, broadcast::Receiver<Arc<Change>>) {
        let inner = self.lock();
        let set = Self::matching(&inner, filter);
        (set, self.changes.subscribe())
    }

    /// The live entries `filter` matches, in id order.
    pub fn current(&self, filter: &PresenceFilter) -> Vec<Arc<Change>> {
        Self::matching(&self.lock(), filter)
    }

    /// Why `id` ended, if it ended recently enough to be remembered.
    pub fn ended(&self, id: PresenceId) -> Option<PresenceEndReason> {
        self.lock().ended_reasons.get(&id).copied()
    }

    /// Every live entry, in id order.
    pub fn entries(&self) -> Vec<v1::Presence> {
        self.lock()
            .entries
            .values()
            .map(|entry| entry.change.presence.clone())
            .collect()
    }

    /// The daemon's live entry `key`.
    pub fn daemon_entry(&self, key: DaemonKey) -> Option<v1::Presence> {
        let inner = self.lock();
        let id = inner.daemon_keys.get(&key)?;
        inner
            .entries
            .get(id)
            .map(|entry| entry.change.presence.clone())
    }

    fn matching(inner: &Inner, filter: &PresenceFilter) -> Vec<Arc<Change>> {
        inner
            .entries
            .values()
            .filter(|entry| filter.matches(&entry.change))
            .map(|entry| Arc::clone(&entry.change))
            .collect()
    }

    /// Stores `report` as entry `id`'s new state, schedules its expiry,
    /// and broadcasts it now or defers the broadcast.
    fn store(
        &self,
        inner: &mut Inner,
        id: PresenceId,
        daemon: Option<DaemonKey>,
        report: Report,
    ) -> Result<v1::Presence, PresenceError> {
        let now = Instant::now();
        let wall = Timestamp::now();
        let presence = v1::Presence {
            id: id.as_hex(),
            agent: report.agent,
            repo_id: report.repo.map(|repo| repo.as_hex()).unwrap_or_default(),
            workspace_id: report
                .workspace
                .map(|workspace| workspace.as_hex())
                .unwrap_or_default(),
            change_id: report.change_id,
            kind: report.kind.into(),
            activity: report.activity,
            paths: report.paths,
            progress: report.progress,
            updated_at: Some(timestamp(wall)),
            expires_at: Some(timestamp(Timestamp(wall.0.saturating_add(
                i64::try_from(report.ttl.as_millis()).unwrap_or(i64::MAX),
            )))),
            lease: report.lease,
            by_daemon: daemon.is_some(),
        };
        if daemon.is_none() && presence.encoded_len() > MAX_ENTRY_BYTES {
            return Err(PresenceError::TooLarge {
                field: "presence",
                limit: "4 KiB".to_owned(),
            });
        }
        inner.seq += 1;
        let change = Arc::new(Change {
            id,
            seq: inner.seq,
            repo: report.repo,
            workspace: report.workspace,
            presence: presence.clone(),
            end: None,
        });
        let expires = now + report.ttl;
        let earliest = inner.deadlines.first().map(|(at, _, _)| *at);
        let entry = match inner.entries.remove(&id) {
            Some(mut entry) => {
                inner.deadlines.remove(&(entry.expires, id, Due::Expire));
                entry.change = Arc::clone(&change);
                entry.expires = expires;
                entry
            }
            None => Entry {
                change: Arc::clone(&change),
                expires,
                daemon,
                broadcast_at: None,
                deferred: None,
            },
        };
        let mut entry = entry;
        inner.deadlines.insert((expires, id, Due::Expire));
        let quiet = entry
            .broadcast_at
            .is_none_or(|at| now >= at + self.limits.publish_interval);
        if entry.deferred.is_some() {
            // The deferred broadcast will carry this state.
        } else if quiet {
            entry.broadcast_at = Some(now);
            let _ = self.changes.send(change);
        } else if let Some(at) = entry.broadcast_at {
            let due = at + self.limits.publish_interval;
            entry.deferred = Some(due);
            inner.deadlines.insert((due, id, Due::Publish));
        }
        inner.entries.insert(id, entry);
        if inner
            .deadlines
            .first()
            .is_some_and(|(at, _, _)| earliest.is_none_or(|earliest| *at < earliest))
        {
            self.wake.notify_one();
        }
        Ok(presence)
    }

    /// Ends live entry `id` for `reason` and broadcasts its last state.
    fn end(&self, inner: &mut Inner, id: PresenceId, reason: PresenceEndReason) {
        let Some(entry) = inner.entries.remove(&id) else {
            return;
        };
        inner.deadlines.remove(&(entry.expires, id, Due::Expire));
        if let Some(at) = entry.deferred {
            inner.deadlines.remove(&(at, id, Due::Publish));
        }
        if let Some(key) = entry.daemon {
            inner.daemon_keys.remove(&key);
        } else {
            inner.clients -= 1;
            let agent = &entry.change.presence.agent;
            if let Some(held) = inner.per_agent.get_mut(agent) {
                *held -= 1;
                if *held == 0 {
                    inner.per_agent.remove(agent);
                }
            }
        }
        inner.ended.push_back(id);
        inner.ended_reasons.insert(id, reason);
        if inner.ended.len() > ENDED_RING
            && let Some(old) = inner.ended.pop_front()
        {
            inner.ended_reasons.remove(&old);
        }
        inner.seq += 1;
        let _ = self
            .changes
            .send(Arc::new(entry.change.ended(inner.seq, reason)));
    }
}

/// An id no live entry has.
fn new_id(inner: &Inner) -> PresenceId {
    loop {
        let id = PresenceId::generate();
        if !inner.entries.contains_key(&id) && !inner.ended_reasons.contains_key(&id) {
            return id;
        }
    }
}

/// Unix milliseconds as a `google.protobuf.Timestamp`.
fn timestamp(time: Timestamp) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: time.0.div_euclid(1000),
        nanos: i32::try_from(time.0.rem_euclid(1000) * 1_000_000).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            max_entries: 5,
            max_entries_per_agent: 3,
            publish_interval: Duration::from_millis(50),
            ..Limits::default()
        }
    }

    fn activity(agent: &str, text: &str) -> Report {
        Report {
            agent: agent.to_owned(),
            kind: PresenceKind::Activity,
            activity: text.to_owned(),
            ttl: Duration::from_secs(30),
            ..Report::default()
        }
    }

    fn id_of(presence: &v1::Presence) -> PresenceId {
        presence.id.parse().unwrap()
    }

    fn drain(rx: &mut broadcast::Receiver<Arc<Change>>) -> Vec<Arc<Change>> {
        let mut out = Vec::new();
        while let Ok(change) = rx.try_recv() {
            out.push(change);
        }
        out
    }

    #[tokio::test(start_paused = true)]
    async fn entries_expire_at_their_deadline() {
        let hub = Arc::new(PresenceHub::new(limits()));
        let stop = CancellationToken::new();
        let task = hub.start(stop.clone());
        let (_, mut rx) = hub.subscribe(&PresenceFilter::default());
        let mut report = activity("a", "x");
        report.ttl = Duration::from_secs(2);
        let presence = hub.report(None, report).unwrap();
        tokio::time::sleep(Duration::from_millis(1_990)).await;
        assert_eq!(hub.entries().len(), 1);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(hub.entries().len(), 0);
        let changes = drain(&mut rx);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[1].end, Some(PresenceEndReason::Expired));
        assert_eq!(
            hub.ended(id_of(&presence)),
            Some(PresenceEndReason::Expired)
        );
        stop.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_replacement_moves_the_expiry() {
        let hub = Arc::new(PresenceHub::new(limits()));
        let stop = CancellationToken::new();
        let task = hub.start(stop.clone());
        let mut report = activity("a", "x");
        report.ttl = Duration::from_secs(2);
        let id = id_of(&hub.report(None, report.clone()).unwrap());
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        hub.report(Some(id), report).unwrap();
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert_eq!(hub.entries().len(), 1, "renewed at 1.5 s, lapses at 3.5 s");
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(hub.entries().len(), 0);
        stop.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn broadcasts_are_coalesced_per_entry() {
        let hub = Arc::new(PresenceHub::new(limits()));
        let stop = CancellationToken::new();
        let task = hub.start(stop.clone());
        let (_, mut rx) = hub.subscribe(&PresenceFilter::default());
        let id = id_of(&hub.report(None, activity("a", "0")).unwrap());
        assert_eq!(drain(&mut rx).len(), 1, "the first goes at once");
        for n in 1..=10 {
            hub.report(Some(id), activity("a", &n.to_string())).unwrap();
        }
        assert_eq!(drain(&mut rx).len(), 0, "the rest wait for the interval");
        tokio::time::sleep(Duration::from_millis(60)).await;
        let changes = drain(&mut rx);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].presence.activity, "10", "the latest state");
        tokio::time::sleep(Duration::from_millis(200)).await;
        hub.report(Some(id), activity("a", "quiet")).unwrap();
        assert_eq!(drain(&mut rx).len(), 1, "after a quiet interval, at once");
        hub.clear(id).unwrap();
        let changes = drain(&mut rx);
        assert_eq!(changes.len(), 1, "ends go at once");
        assert_eq!(changes[0].end, Some(PresenceEndReason::Cleared));
        stop.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn caps_refuse_and_never_count_daemon_entries() {
        let hub = PresenceHub::new(limits());
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(id_of(&hub.report(None, activity("a", "x")).unwrap()));
        }
        let error = hub.report(None, activity("a", "x")).unwrap_err();
        assert!(matches!(
            error,
            PresenceError::TooMany {
                limit: 3,
                agent: Some(_)
            }
        ));
        hub.report(None, activity("b", "x")).unwrap();
        hub.report(None, activity("b", "x")).unwrap();
        let error = hub.report(None, activity("c", "x")).unwrap_err();
        assert!(matches!(
            error,
            PresenceError::TooMany {
                limit: 5,
                agent: None
            }
        ));
        hub.publish_daemon(DaemonKey::Progress(1), |_| activity("d", "x"));
        hub.clear(ids[0]).unwrap();
        hub.report(None, activity("c", "x")).unwrap();
    }

    #[tokio::test]
    async fn fixed_fields_and_daemon_entries_are_refused() {
        let hub = PresenceHub::new(limits());
        let id = id_of(&hub.report(None, activity("a", "x")).unwrap());
        let error = hub.report(Some(id), activity("b", "x")).unwrap_err();
        assert!(matches!(
            error,
            PresenceError::Mismatch { field: "agent", .. }
        ));
        let mut editing = activity("a", "");
        editing.kind = PresenceKind::Editing;
        let error = hub.report(Some(id), editing).unwrap_err();
        assert!(matches!(
            error,
            PresenceError::Mismatch { field: "kind", .. }
        ));
        let mut lease = activity("a", "");
        lease.kind = PresenceKind::Lease;
        assert!(matches!(
            hub.report(None, lease).unwrap_err(),
            PresenceError::DaemonKind
        ));
        let daemon = hub.publish_daemon(DaemonKey::Progress(1), |_| activity("d", "x"));
        assert!(matches!(
            hub.report(Some(daemon), activity("d", "y")).unwrap_err(),
            PresenceError::DaemonEntry(_)
        ));
        assert!(matches!(
            hub.clear(daemon).unwrap_err(),
            PresenceError::DaemonEntry(_)
        ));
        hub.clear(id).unwrap();
        hub.clear(id).unwrap();
        assert!(matches!(
            hub.report(Some(id), activity("a", "x")).unwrap_err(),
            PresenceError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn daemon_entries_are_replaced_by_key() {
        let hub = PresenceHub::new(limits());
        let ws = WorkspaceId::generate();
        let first = hub.publish_daemon(DaemonKey::Editing(ws), |previous| {
            assert!(previous.is_none());
            Report {
                paths: vec!["a".to_owned()],
                ..activity("o", "")
            }
        });
        let second = hub.publish_daemon(DaemonKey::Editing(ws), |previous| {
            assert_eq!(previous.unwrap().paths, ["a"]);
            Report {
                paths: vec!["b".to_owned(), "a".to_owned()],
                ..activity("o", "")
            }
        });
        assert_eq!(first, second);
        assert_eq!(
            hub.daemon_entry(DaemonKey::Editing(ws)).unwrap().paths,
            ["b", "a"]
        );
        hub.end_daemon(DaemonKey::Editing(ws), PresenceEndReason::Cleared);
        assert!(hub.daemon_entry(DaemonKey::Editing(ws)).is_none());
    }

    #[tokio::test]
    async fn workspaces_and_repositories_end_their_entries() {
        let hub = PresenceHub::new(limits());
        let (repo, ws) = (RepoId::generate(), WorkspaceId::generate());
        let (_, mut rx) = hub.subscribe(&PresenceFilter::default());
        let in_ws = Report {
            repo: Some(repo),
            workspace: Some(ws),
            ..activity("a", "x")
        };
        let in_repo = Report {
            repo: Some(repo),
            ..activity("a", "y")
        };
        hub.report(None, in_ws.clone()).unwrap();
        hub.report(None, in_repo).unwrap();
        hub.publish_daemon(DaemonKey::Lease(ws), |_| Report {
            kind: PresenceKind::Lease,
            ..in_ws.clone()
        });
        hub.end_workspace(ws);
        assert_eq!(hub.entries().len(), 1);
        assert!(matches!(
            hub.report(None, in_ws.clone()).unwrap_err(),
            PresenceError::Ended
        ));
        hub.publish_daemon(DaemonKey::Lease(ws), |_| in_ws.clone());
        assert_eq!(
            hub.entries().len(),
            1,
            "no daemon entry for an ended workspace"
        );
        hub.end_repository(repo);
        assert_eq!(hub.entries().len(), 0);
        let ends: Vec<_> = drain(&mut rx)
            .into_iter()
            .filter_map(|change| change.end)
            .collect();
        assert_eq!(ends, [PresenceEndReason::Gone; 3]);
    }

    #[tokio::test]
    async fn a_report_racing_a_forget_is_refused() {
        let hub = PresenceHub::new(limits());
        let repo = RepoId::generate();
        hub.end_repository(repo);
        let in_repo = Report {
            repo: Some(repo),
            ..activity("a", "x")
        };
        assert!(matches!(
            hub.report(None, in_repo.clone()).unwrap_err(),
            PresenceError::Ended
        ));
        hub.publish_daemon(DaemonKey::Progress(1), |_| in_repo);
        assert_eq!(hub.entries().len(), 0);
    }

    #[tokio::test]
    async fn oversized_client_entries_are_refused() {
        let hub = PresenceHub::new(limits());
        let report = Report {
            kind: PresenceKind::Editing,
            paths: (0..64).map(|n| format!("{n:0>70}")).collect(),
            ..activity("a", "")
        };
        assert!(matches!(
            hub.report(None, report).unwrap_err(),
            PresenceError::TooLarge { .. }
        ));
        assert_eq!(hub.entries().len(), 0);
    }
}
