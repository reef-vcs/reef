//! Reporting presence for a process (R2.11): the contract
//! `ReportPresence`'s comment states, implemented once. `ws run` reports
//! the command it runs with it, `reef mcp` the tools it serves, and the
//! SDKs (R2.06, R2.07) port it.
//!
//! A [`Reporter`] holds one entry for one [`Subject`]: it replaces the
//! entry by id at most once a second ([`MIN_INTERVAL`]), refreshes it
//! before its TTL runs out while the activity lasts ([`Cadence::REFRESH`]),
//! reports it again without an id when the daemon no longer knows it, and
//! clears it within [`CLEAR_DEADLINE`]. Presence never fails the work: a
//! report that fails is logged at `debug` and tried again, and one refused
//! because its subject is gone stops the reporter.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use reef_api::v1::{PresenceKind, Progress, ReportPresenceRequest, ReportPresenceResponse};
use reef_error::{Classify as _, ErrorChain, ErrorKind};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::{Client, ClientError};

/// How often a long-lived reporter replaces its entry.
pub const REFRESH_EVERY: Duration = Duration::from_secs(10);
/// A long-lived reporter's TTL: three refreshes, the daemon's default.
pub const REFRESH_TTL: Duration = Duration::from_secs(30);
/// A bursty reporter's TTL, sent with each report.
pub const BURST_TTL: Duration = Duration::from_secs(60);
/// The least time between two reports of one entry.
pub const MIN_INTERVAL: Duration = Duration::from_secs(1);
/// How long [`Reporter::clear`] waits for the daemon in all.
pub const CLEAR_DEADLINE: Duration = Duration::from_secs(2);
/// Each report's deadline, as each heartbeat's.
const CALL_DEADLINE: Duration = Duration::from_secs(5);

/// The presence side of the client as a trait, so the reporter runs
/// against a fake in tests, as the heartbeater does over
/// [`crate::WorkspacesApi`].
#[tonic::async_trait]
pub trait PresenceApi: Send + Sync {
    /// `PresenceService.ReportPresence`.
    async fn report_presence(
        &self,
        request: ReportPresenceRequest,
    ) -> Result<ReportPresenceResponse, ClientError>;
    /// `PresenceService.ClearPresence`.
    async fn clear_presence(&self, presence_id: &str) -> Result<(), ClientError>;
}

#[tonic::async_trait]
impl PresenceApi for Client {
    async fn report_presence(
        &self,
        request: ReportPresenceRequest,
    ) -> Result<ReportPresenceResponse, ClientError> {
        Client::report_presence(self, request).await
    }

    async fn clear_presence(&self, presence_id: &str) -> Result<(), ClientError> {
        Client::clear_presence(self, presence_id).await
    }
}

/// An entry's fixed fields: a replacement must keep them, or the daemon
/// refuses it with `PRESENCE_MISMATCH`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Subject {
    /// Who reports, by [`agent`]'s rule.
    pub agent: String,
    /// The repository, or empty when `workspace` names it.
    pub repo: String,
    /// The workspace, or empty for repository scope.
    pub workspace: String,
    /// Any kind but `PRESENCE_KIND_LEASE`.
    pub kind: PresenceKind,
}

/// What an entry says: everything a replacement may change.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    /// A full change id in hex, or empty.
    pub change: String,
    /// For `PRESENCE_KIND_ACTIVITY`.
    pub activity: String,
    /// Repository-relative, for `PRESENCE_KIND_EDITING`.
    pub paths: Vec<String>,
    /// For `PRESENCE_KIND_PROGRESS`.
    pub progress: Option<Progress>,
}

/// When a reporter replaces its entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cadence {
    /// For an activity that lasts (`ws run`'s command): replaced every
    /// `every` with a TTL of `ttl`, so a reporter that dies leaves an entry
    /// that expires within `ttl`.
    Refresh {
        /// The interval between refreshes.
        every: Duration,
        /// Each report's TTL.
        ttl: Duration,
    },
    /// For activity in bursts (`reef mcp`'s calls): one report per
    /// [`Reporter::set`], never refreshed, so the entry lapsing says the
    /// agent went quiet.
    Burst {
        /// Each report's TTL.
        ttl: Duration,
    },
}

impl Cadence {
    /// [`REFRESH_EVERY`] with [`REFRESH_TTL`].
    pub const REFRESH: Self = Self::Refresh {
        every: REFRESH_EVERY,
        ttl: REFRESH_TTL,
    };
    /// [`BURST_TTL`].
    pub const BURST: Self = Self::Burst { ttl: BURST_TTL };

    fn ttl(self) -> Duration {
        match self {
            Self::Refresh { ttl, .. } | Self::Burst { ttl } => ttl,
        }
    }

    fn every(self) -> Option<Duration> {
        match self {
            Self::Refresh { every, .. } => Some(every),
            Self::Burst { .. } => None,
        }
    }
}

/// The agent an entry names: the name the client sends as `reef-agent`,
/// else the owner of the workspace the entry names, else `fallback`. A
/// blank name, or one the daemon would refuse as an agent (an owner need
/// not be visible ASCII, nor at most 256 bytes), falls through.
pub fn agent(given: Option<&str>, owner: Option<&str>, fallback: &str) -> String {
    [given, owner]
        .into_iter()
        .flatten()
        .find_map(|name| crate::check_agent(name).ok().flatten())
        .unwrap_or(fallback)
        .to_owned()
}

/// What the reporter's task and its handle share.
#[derive(Default)]
struct Shared {
    inner: Mutex<Inner>,
    wake: Notify,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // one flag each, read under one lock
struct Inner {
    /// The latest state [`Reporter::set`] gave.
    state: Option<State>,
    /// Set since the last report.
    pending: bool,
    /// A report is in flight.
    sending: bool,
    /// The entry the daemon gave, while held.
    id: Option<String>,
    /// The last report's TTL from when it was answered.
    live_until: Option<Instant>,
    closing: bool,
    stopped: bool,
}

/// Holds one presence entry for one subject. Nothing is sent until the
/// first [`Reporter::set`]; dropping it stops it without a clear, leaving
/// the entry to its TTL.
pub struct Reporter {
    shared: Arc<Shared>,
    task: Option<JoinHandle<()>>,
}

impl Reporter {
    /// Starts the reporter's task on the current runtime.
    pub fn start(api: Arc<dyn PresenceApi>, subject: Subject, cadence: Cadence) -> Self {
        let shared = Arc::new(Shared::default());
        let task = tokio::spawn(run(api, subject, cadence, Arc::clone(&shared)));
        Self {
            shared,
            task: Some(task),
        }
    }

    /// What the entry says from now on: sent at once if the last report was
    /// [`MIN_INTERVAL`] ago or more, else at that interval's end with
    /// whatever was set last.
    pub fn set(&self, state: State) {
        let mut inner = self.shared.lock();
        if inner.stopped || inner.closing {
            return;
        }
        inner.state = Some(state);
        inner.pending = true;
        drop(inner);
        self.shared.wake.notify_one();
    }

    /// Whether it may still hold a live entry: not stopped, and a report is
    /// due or in flight or the last one's TTL has not run out.
    pub fn is_live(&self) -> bool {
        let inner = self.shared.lock();
        !inner.stopped
            && (inner.pending
                || inner.sending
                || inner.live_until.is_some_and(|at| at > Instant::now()))
    }

    /// Stops the reporter and clears its entry, waiting at most
    /// [`CLEAR_DEADLINE`] for a report in flight and the clear.
    pub async fn clear(mut self) {
        self.shared.lock().closing = true;
        self.shared.wake.notify_one();
        if let Some(mut task) = self.task.take()
            && tokio::time::timeout(CLEAR_DEADLINE, &mut task)
                .await
                .is_err()
        {
            task.abort();
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// What the task does next.
enum Next {
    Send,
    Wait(Option<Instant>),
    Close(Option<String>),
}

async fn run(api: Arc<dyn PresenceApi>, subject: Subject, cadence: Cadence, shared: Arc<Shared>) {
    // When the last report was sent, for the interval and the refresh.
    let mut last: Option<Instant> = None;
    loop {
        let now = Instant::now();
        let next = {
            let inner = shared.lock();
            let due = |after: Duration| match last {
                Some(at) if now < at + after => Next::Wait(Some(at + after)),
                _ => Next::Send,
            };
            if inner.closing {
                Next::Close(inner.id.clone())
            } else if inner.pending {
                due(MIN_INTERVAL)
            } else if let (Some(every), Some(_)) = (cadence.every(), &inner.state) {
                due(every)
            } else {
                Next::Wait(None)
            }
        };
        match next {
            Next::Close(id) => {
                if let Some(id) = id {
                    match tokio::time::timeout(CALL_DEADLINE, api.clear_presence(&id)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            tracing::debug!(presence_id = %id, error = %ErrorChain(&error), "presence clear failed");
                        }
                        Err(_) => tracing::debug!(presence_id = %id, "presence clear timed out"),
                    }
                }
                return;
            }
            Next::Wait(deadline) => {
                tokio::select! {
                    () = shared.wake.notified() => {}
                    () = sleep_until(deadline) => {}
                }
            }
            Next::Send => {
                let request = {
                    let mut inner = shared.lock();
                    inner.pending = false;
                    inner.sending = true;
                    request(&subject, cadence, inner.id.as_deref(), inner.state.as_ref())
                };
                last = Some(now);
                let replacing = !request.presence_id.is_empty();
                let answer =
                    tokio::time::timeout(CALL_DEADLINE, api.report_presence(request)).await;
                let mut inner = shared.lock();
                inner.sending = false;
                match answer {
                    Ok(Ok(response)) => {
                        if let Some(presence) = response.presence.filter(|p| !p.id.is_empty()) {
                            inner.id = Some(presence.id);
                        }
                        inner.live_until = Some(Instant::now() + cadence.ttl());
                    }
                    Ok(Err(error))
                        if replacing && error.reason() == reef_api::error::PRESENCE_NOT_FOUND =>
                    {
                        // Lapsed, or from before a restart: report it anew
                        // at once.
                        inner.id = None;
                        inner.live_until = None;
                        inner.pending = true;
                        last = None;
                    }
                    Ok(Err(error)) if ends_reporting(&error) => {
                        tracing::debug!(error = %ErrorChain(&error), "presence reporting stopped");
                        inner.stopped = true;
                        inner.id = None;
                        return;
                    }
                    Ok(Err(error)) => {
                        tracing::debug!(error = %ErrorChain(&error), "presence report failed");
                    }
                    // A first report that timed out may still have made
                    // an entry the reporter never learns of; it lapses
                    // after its TTL.
                    Err(_) => tracing::debug!("presence report timed out"),
                }
            }
        }
    }
}

/// Whether a refusal means reporting again cannot succeed: the subject is
/// gone (a workspace that ended, `Conflict`; an unknown workspace or
/// repository, `NotFound`), the daemon does not serve presence, or the
/// report itself is wrong.
fn ends_reporting(error: &ClientError) -> bool {
    matches!(
        error.kind(),
        ErrorKind::Conflict
            | ErrorKind::NotFound
            | ErrorKind::Unsupported
            | ErrorKind::InvalidArgument
            | ErrorKind::PermissionDenied
    )
}

fn request(
    subject: &Subject,
    cadence: Cadence,
    id: Option<&str>,
    state: Option<&State>,
) -> ReportPresenceRequest {
    let state = state.cloned().unwrap_or_default();
    ReportPresenceRequest {
        presence_id: id.unwrap_or_default().to_owned(),
        agent: subject.agent.clone(),
        repo: subject.repo.clone(),
        workspace: subject.workspace.clone(),
        change: state.change,
        kind: subject.kind.into(),
        activity: state.activity,
        paths: state.paths,
        progress: state.progress,
        ttl: Some(crate::proto_duration(cadence.ttl())),
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use reef_api::v1::Presence;

    use super::*;

    /// One scripted answer.
    enum Answer {
        Ok,
        Fail(tonic::Status),
        Hang,
    }

    /// A request the fake saw, reports with when.
    #[derive(Debug)]
    enum Seen {
        Report(u64, Box<ReportPresenceRequest>),
        Clear(String),
    }

    /// A daemon that answers reports from a script, then with success, and
    /// gives each new entry the next id.
    struct Fake {
        start: Instant,
        answers: Mutex<VecDeque<Answer>>,
        seen: Mutex<Vec<Seen>>,
        ids: Mutex<u32>,
    }

    impl Fake {
        fn new(answers: Vec<Answer>) -> Arc<Self> {
            Arc::new(Self {
                start: Instant::now(),
                answers: Mutex::new(answers.into()),
                seen: Mutex::new(Vec::new()),
                ids: Mutex::new(0),
            })
        }

        fn reports(&self) -> Vec<(u64, ReportPresenceRequest)> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter_map(|seen| match seen {
                    Seen::Report(at, request) => Some((*at, (**request).clone())),
                    Seen::Clear(..) => None,
                })
                .collect()
        }

        fn clears(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter_map(|seen| match seen {
                    Seen::Clear(id) => Some(id.clone()),
                    Seen::Report(..) => None,
                })
                .collect()
        }

        fn next_answer(&self) -> Answer {
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Answer::Ok)
        }
    }

    #[tonic::async_trait]
    impl PresenceApi for Fake {
        async fn report_presence(
            &self,
            request: ReportPresenceRequest,
        ) -> Result<ReportPresenceResponse, ClientError> {
            let at = u64::try_from(self.start.elapsed().as_millis()).unwrap();
            self.seen
                .lock()
                .unwrap()
                .push(Seen::Report(at, Box::new(request.clone())));
            match self.next_answer() {
                Answer::Ok => {
                    let id = if request.presence_id.is_empty() {
                        let mut ids = self.ids.lock().unwrap();
                        *ids += 1;
                        format!("{:016x}", *ids)
                    } else {
                        request.presence_id
                    };
                    Ok(ReportPresenceResponse {
                        presence: Some(Presence {
                            id,
                            ..Presence::default()
                        }),
                    })
                }
                Answer::Fail(status) => Err(status.into()),
                Answer::Hang => std::future::pending().await,
            }
        }

        async fn clear_presence(&self, presence_id: &str) -> Result<(), ClientError> {
            self.seen
                .lock()
                .unwrap()
                .push(Seen::Clear(presence_id.to_owned()));
            match self.next_answer() {
                Answer::Hang => std::future::pending().await,
                _ => Ok(()),
            }
        }
    }

    fn subject() -> Subject {
        Subject {
            agent: "bot-1".into(),
            workspace: "ws".into(),
            kind: PresenceKind::Activity,
            ..Subject::default()
        }
    }

    fn doing(activity: &str) -> State {
        State {
            activity: activity.into(),
            ..State::default()
        }
    }

    /// A status carrying an `ErrorDetail` of `kind` with `reason`.
    fn refused(code: tonic::Code, kind: reef_api::v1::ErrorKind, reason: &str) -> tonic::Status {
        reef_api::error::status(
            code,
            "refused".to_owned(),
            &reef_api::v1::ErrorDetail {
                kind: kind.into(),
                reason: reason.into(),
                ..reef_api::v1::ErrorDetail::default()
            },
        )
    }

    /// Lets the reporter's task run to its next wait.
    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    /// Moves the paused clock on by `by`, stopping at each timer on the
    /// way so the reporter wakes when it asked to.
    async fn advance(by: Duration) {
        tokio::time::sleep(by).await;
        settle().await;
    }

    // 1
    #[tokio::test(start_paused = true)]
    async fn refresh_reports_replaces_and_clears() {
        let fake = Fake::new(vec![]);
        let reporter = Reporter::start(fake.clone(), subject(), Cadence::REFRESH);
        advance(Duration::from_secs(5)).await;
        assert!(fake.reports().is_empty(), "nothing before set");
        reporter.set(doing("running make"));
        settle().await;
        advance(Duration::from_secs(10)).await;
        advance(Duration::from_secs(10)).await;
        let reports = fake.reports();
        assert_eq!(reports.len(), 3, "{reports:?}");
        let (at, first) = &reports[0];
        assert_eq!(*at, 5_000);
        assert_eq!(first.presence_id, "");
        assert_eq!(first.agent, "bot-1");
        assert_eq!(first.workspace, "ws");
        assert_eq!(first.kind, i32::from(PresenceKind::Activity));
        assert_eq!(first.activity, "running make");
        assert_eq!(first.ttl, Some(crate::proto_duration(REFRESH_TTL)));
        for (at, (when, replacement)) in [15_000, 25_000].iter().zip(&reports[1..]) {
            assert_eq!(when, at);
            assert_eq!(replacement.presence_id, "0000000000000001");
            assert_eq!(replacement.activity, "running make");
        }
        assert!(reporter.is_live());
        reporter.clear().await;
        assert_eq!(fake.clears(), ["0000000000000001"]);
        advance(Duration::from_secs(60)).await;
        assert_eq!(fake.reports().len(), 3, "nothing after clear");
    }

    // 2
    #[tokio::test(start_paused = true)]
    async fn rereports_after_not_found() {
        let fake = Fake::new(vec![
            Answer::Ok,
            Answer::Fail(refused(
                tonic::Code::NotFound,
                reef_api::v1::ErrorKind::NotFound,
                reef_api::error::PRESENCE_NOT_FOUND,
            )),
        ]);
        let reporter = Reporter::start(fake.clone(), subject(), Cadence::REFRESH);
        reporter.set(doing("a"));
        settle().await;
        advance(REFRESH_EVERY).await;
        let reports = fake.reports();
        assert_eq!(reports.len(), 3, "{reports:?}");
        assert_eq!(reports[1].1.presence_id, "0000000000000001");
        assert_eq!(reports[2].0, reports[1].0, "reported anew at once");
        assert_eq!(reports[2].1.presence_id, "");
        assert_eq!(reports[2].1.activity, "a");
        advance(REFRESH_EVERY).await;
        assert_eq!(fake.reports()[3].1.presence_id, "0000000000000002");
        reporter.clear().await;
        assert_eq!(fake.clears(), ["0000000000000002"]);
    }

    // 3
    #[tokio::test(start_paused = true)]
    async fn coalesces_within_a_second() {
        let fake = Fake::new(vec![]);
        let reporter = Reporter::start(fake.clone(), subject(), Cadence::REFRESH);
        for n in 0..10 {
            reporter.set(doing(&format!("step {n}")));
            settle().await;
            advance(Duration::from_millis(30)).await;
        }
        advance(Duration::from_secs(1)).await;
        let reports = fake.reports();
        assert_eq!(reports.len(), 2, "{reports:?}");
        assert_eq!(
            (reports[0].0, reports[0].1.activity.as_str()),
            (0, "step 0")
        );
        assert_eq!(
            (reports[1].0, reports[1].1.activity.as_str()),
            (1_000, "step 9")
        );
        advance(Duration::from_secs(2)).await;
        reporter.set(doing("later"));
        settle().await;
        let reports = fake.reports();
        assert_eq!(reports.len(), 3, "{reports:?}");
        assert_eq!(
            (reports[2].0, reports[2].1.activity.as_str()),
            (3_300, "later")
        );
    }

    // 4
    #[tokio::test(start_paused = true)]
    async fn failures_do_not_stop_it() {
        let fake = Fake::new(vec![
            Answer::Fail(tonic::Status::unavailable("daemon is down")),
            Answer::Fail(refused(
                tonic::Code::Unavailable,
                reef_api::v1::ErrorKind::Unavailable,
                reef_api::error::TOO_MANY_PRESENCE_ENTRIES,
            )),
            Answer::Ok,
        ]);
        let reporter = Reporter::start(fake.clone(), subject(), Cadence::REFRESH);
        reporter.set(doing("a"));
        settle().await;
        advance(REFRESH_EVERY).await;
        advance(REFRESH_EVERY).await;
        let reports = fake.reports();
        assert_eq!(reports.len(), 3, "retried at each refresh: {reports:?}");
        assert!(reports.iter().all(|(_, r)| r.presence_id.is_empty()));
        assert!(reporter.is_live());

        for status in [
            tonic::Status::failed_precondition("workspace is released"),
            refused(
                tonic::Code::NotFound,
                reef_api::v1::ErrorKind::NotFound,
                reef_api::error::WORKSPACE_NOT_FOUND,
            ),
            tonic::Status::unimplemented("no presence here"),
        ] {
            let fake = Fake::new(vec![Answer::Fail(status)]);
            let reporter = Reporter::start(fake.clone(), subject(), Cadence::REFRESH);
            reporter.set(doing("a"));
            settle().await;
            advance(REFRESH_EVERY * 3).await;
            reporter.set(doing("b"));
            settle().await;
            assert_eq!(fake.reports().len(), 1, "stopped for good");
            assert!(!reporter.is_live());
            reporter.clear().await;
            assert_eq!(fake.clears(), Vec::<String>::new());
        }
    }

    // 4, with an entry held: an unreachable daemon keeps the id, and a
    // refusal because the workspace ended stops without a clear.
    #[tokio::test(start_paused = true)]
    async fn failures_with_an_entry_held() {
        let fake = Fake::new(vec![
            Answer::Ok,
            Answer::Fail(tonic::Status::unavailable("daemon is down")),
            Answer::Ok,
            Answer::Fail(refused(
                tonic::Code::FailedPrecondition,
                reef_api::v1::ErrorKind::Conflict,
                "",
            )),
        ]);
        let reporter = Reporter::start(fake.clone(), subject(), Cadence::REFRESH);
        reporter.set(doing("a"));
        settle().await;
        advance(REFRESH_EVERY).await;
        advance(REFRESH_EVERY).await;
        advance(REFRESH_EVERY).await;
        advance(REFRESH_EVERY * 3).await;
        let reports = fake.reports();
        assert_eq!(reports.len(), 4, "{reports:?}");
        for (_, replacement) in &reports[1..] {
            assert_eq!(replacement.presence_id, "0000000000000001");
        }
        assert!(!reporter.is_live());
        reporter.clear().await;
        assert_eq!(fake.clears(), Vec::<String>::new(), "the daemon ended it");
    }

    // 4, bursts: a report refused at the cap waits for the next set.
    #[tokio::test(start_paused = true)]
    async fn burst_drops_a_refused_report() {
        let fake = Fake::new(vec![Answer::Fail(refused(
            tonic::Code::Unavailable,
            reef_api::v1::ErrorKind::Unavailable,
            reef_api::error::TOO_MANY_PRESENCE_ENTRIES,
        ))]);
        let reporter = Reporter::start(fake.clone(), subject(), Cadence::BURST);
        reporter.set(doing("get_status"));
        settle().await;
        advance(Duration::from_secs(30)).await;
        assert_eq!(fake.reports().len(), 1);
        reporter.set(doing("list_files"));
        settle().await;
        let reports = fake.reports();
        assert_eq!(reports.len(), 2, "{reports:?}");
        assert_eq!(reports[1].1.presence_id, "");
        assert_eq!(reports[1].1.activity, "list_files");
    }

    // 5
    #[tokio::test(start_paused = true)]
    async fn burst_does_not_refresh() {
        let fake = Fake::new(vec![]);
        let reporter = Reporter::start(fake.clone(), subject(), Cadence::BURST);
        reporter.set(doing("get_status"));
        settle().await;
        advance(Duration::from_secs(5)).await;
        reporter.set(doing("list_files"));
        settle().await;
        advance(Duration::from_secs(50)).await;
        let reports = fake.reports();
        assert_eq!(reports.len(), 2, "{reports:?}");
        assert_eq!(reports[0].1.ttl, Some(crate::proto_duration(BURST_TTL)));
        assert_eq!(reports[1].1.presence_id, "0000000000000001");
        assert!(reporter.is_live());
        advance(Duration::from_secs(11)).await;
        assert!(!reporter.is_live(), "60 s after the last report");
        assert_eq!(fake.reports().len(), 2);
    }

    // 6
    #[tokio::test(start_paused = true)]
    async fn clear_is_bounded() {
        let fake = Fake::new(vec![Answer::Ok, Answer::Hang]);
        let reporter = Reporter::start(fake.clone(), subject(), Cadence::REFRESH);
        reporter.set(doing("a"));
        settle().await;
        let start = Instant::now();
        reporter.clear().await;
        assert!(start.elapsed() <= CLEAR_DEADLINE, "{:?}", start.elapsed());
        assert_eq!(fake.clears(), ["0000000000000001"]);

        // A report that hangs is not waited for past the deadline either.
        let fake = Fake::new(vec![Answer::Hang]);
        let reporter = Reporter::start(fake.clone(), subject(), Cadence::REFRESH);
        reporter.set(doing("a"));
        settle().await;
        let start = Instant::now();
        reporter.clear().await;
        assert!(start.elapsed() <= CLEAR_DEADLINE, "{:?}", start.elapsed());

        let fake = Fake::new(vec![]);
        let reporter = Reporter::start(fake.clone(), subject(), Cadence::REFRESH);
        reporter.clear().await;
        assert_eq!(fake.reports().len(), 0, "nothing before set");
        assert!(fake.clears().is_empty(), "no entry held");

        // A set the task has not yet sent is dropped by a clear.
        let fake = Fake::new(vec![]);
        let reporter = Reporter::start(fake.clone(), subject(), Cadence::REFRESH);
        reporter.set(doing("a"));
        reporter.clear().await;
        assert_eq!(fake.reports().len(), 0, "never sent");
        assert!(fake.clears().is_empty(), "no entry held");
    }

    // 7
    #[test]
    fn agent_rule() {
        assert_eq!(agent(Some("bot"), Some("u@h:1"), "u@h"), "bot");
        assert_eq!(agent(None, Some("u@h:1"), "u@h"), "u@h:1");
        assert_eq!(agent(Some("  "), Some("u@h:1"), "u@h"), "u@h:1");
        assert_eq!(agent(None, None, "u@h"), "u@h");
        assert_eq!(agent(Some(""), Some(""), "u@h"), "u@h");
        assert_eq!(agent(None, Some("équipe"), "u@h"), "u@h", "not ASCII");
        assert_eq!(
            agent(None, Some(&"x".repeat(300)), "u@h"),
            "u@h",
            "too long"
        );
    }
}
