//! Keeping a lease alive: `ws run`'s heartbeats for its workspace (R1.21),
//! and `reef mcp`'s for the workspaces it creates (R2.13).

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use reef_api::v1::{Lease, RenewLeaseResponse, Workspace};
use reef_client::{ClientError, WorkspacesApi, json};
use reef_error::{Classify as _, ErrorChain, ErrorKind};

use super::CliError;
use super::repo::missing;

/// Each heartbeat's deadline.
const HEARTBEAT_DEADLINE: Duration = Duration::from_secs(5);
/// The longest wait between heartbeats while the daemon is unreachable.
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// Renews the lease every `heartbeat_every` until aborted, or until the
/// daemon says the lease has ended, which it reports once.
pub(crate) async fn heartbeats(mut heartbeater: Heartbeater) {
    loop {
        tokio::time::sleep(heartbeater.next_delay()).await;
        if let Err(error) = heartbeater.tick().await {
            heartbeater.notify(&format!(
                "reef: lease {} cannot be renewed: {}; the workspace may be reclaimed",
                heartbeater.lease,
                ErrorChain(&error)
            ));
            return;
        }
    }
}

/// Whether the last heartbeat reached the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkState {
    Up,
    Lost,
}

/// Retry delays while the daemon is unreachable: 1, 2, 4, 8, 16, then 30 s.
#[derive(Debug, Default)]
struct Backoff {
    failures: u32,
}

impl Backoff {
    fn fail(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }

    fn reset(&mut self) {
        self.failures = 0;
    }

    fn delay(&self) -> Duration {
        let doubling = self.failures.saturating_sub(1).min(5);
        Duration::from_secs(1 << doubling).min(BACKOFF_CAP)
    }
}

/// Keeps one lease alive: renews it on the daemon's `heartbeat_every`, and
/// while the daemon is unreachable retries with [`Backoff`], noting each
/// change of [`LinkState`] once, and once that the lease has run out.
pub(crate) struct Heartbeater {
    api: Arc<dyn WorkspacesApi>,
    workspace: String,
    lease: String,
    every: Duration,
    grace: Duration,
    /// `expires_at + grace` as of the last answer: past it, the daemon may
    /// reclaim the workspace.
    lapses_at: Option<SystemTime>,
    warned_lapsed: bool,
    backoff: Backoff,
    state: LinkState,
    notify: Box<dyn Fn(&str) + Send + Sync>,
}

impl Heartbeater {
    /// For `lease` on `ws`, with the cadence the daemon gave it.
    pub(crate) fn new(
        api: Arc<dyn WorkspacesApi>,
        ws: &Workspace,
        lease: &Lease,
        notify: Box<dyn Fn(&str) + Send + Sync>,
    ) -> Result<Self, CliError> {
        let every = lease
            .heartbeat_every
            .and_then(|every| Duration::try_from(every).ok())
            .filter(|every| !every.is_zero())
            .ok_or_else(|| missing("lease.heartbeat_every"))?;
        let grace = Duration::from_millis(lease.grace.as_ref().map_or(0, json::duration_millis));
        Ok(Self {
            api,
            workspace: ws.id.clone(),
            lease: lease.id.clone(),
            every,
            grace,
            lapses_at: lapse(lease.expires_at, grace),
            warned_lapsed: false,
            backoff: Backoff::default(),
            state: LinkState::Up,
            notify,
        })
    }

    /// How long to wait before the next [`Heartbeater::tick`].
    fn next_delay(&self) -> Duration {
        match self.state {
            LinkState::Up => self.every,
            LinkState::Lost => self.backoff.delay(),
        }
    }

    /// One heartbeat. `Ok(None)` while it fails for any reason but the
    /// lease being gone; an error when it is: `Conflict` (ended, replaced,
    /// or past its grace) or `NotFound`. A request cut off by a daemon
    /// restart fails as `Internal`, which must not stop the heartbeats.
    async fn tick(&mut self) -> Result<Option<RenewLeaseResponse>, ClientError> {
        let call = self.api.renew_lease("", &self.workspace, &self.lease);
        let reason = match tokio::time::timeout(HEARTBEAT_DEADLINE, call).await {
            Ok(Ok(response)) => {
                self.renewed(&response);
                return Ok(Some(response));
            }
            Ok(Err(error)) if lease_gone(&error) => return Err(error),
            Ok(Err(error)) => ErrorChain(&error).to_string(),
            Err(_) => format!(
                "no answer within {}",
                humantime::format_duration(HEARTBEAT_DEADLINE)
            ),
        };
        self.unreachable(&reason);
        Ok(None)
    }

    fn renewed(&mut self, response: &RenewLeaseResponse) {
        let Some(lease) = response.lease.as_ref() else {
            return;
        };
        if self.state == LinkState::Lost {
            self.notify("reef: daemon reachable again; lease renewed");
        }
        self.state = LinkState::Up;
        self.backoff.reset();
        if let Some(every) = lease
            .heartbeat_every
            .and_then(|every| Duration::try_from(every).ok())
            .filter(|every| !every.is_zero())
        {
            self.every = every;
        }
        self.lapses_at = lapse(lease.expires_at, self.grace).or(self.lapses_at);
        self.warned_lapsed = false;
    }

    fn unreachable(&mut self, reason: &str) {
        if self.state == LinkState::Up {
            self.notify(&format!(
                "reef: daemon unreachable ({reason}); retrying, and the command keeps running"
            ));
        }
        self.state = LinkState::Lost;
        self.backoff.fail();
        let lapsed = self.lapses_at.is_some_and(|at| SystemTime::now() > at);
        if lapsed && !self.warned_lapsed {
            self.warned_lapsed = true;
            self.notify(&format!(
                "reef: lease {} has expired; the daemon may reclaim workspace {} and remove its directory",
                self.lease, self.workspace
            ));
        }
    }

    fn notify(&self, text: &str) {
        (self.notify)(text);
    }
}

/// Whether a heartbeat failed because the lease can no longer be renewed.
pub(crate) fn lease_gone(error: &ClientError) -> bool {
    matches!(error.kind(), ErrorKind::Conflict | ErrorKind::NotFound)
}

/// `expires_at + grace`. Generic over the API's timestamp type, which this
/// crate does not name.
fn lapse<T>(expires_at: Option<T>, grace: Duration) -> Option<SystemTime>
where
    SystemTime: TryFrom<T>,
{
    let expires_at = SystemTime::try_from(expires_at?).ok()?;
    expires_at.checked_add(grace)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;

    use reef_api::v1::IfNotJj;
    use reef_api::v1::{
        CreateWorkspaceRequest, CreateWorkspaceResponse, ForgetRepositoryResponse,
        GetDaemonStatusResponse, GetWorkspaceResponse, ListWorkspacesRequest,
        RegisterRepositoryResponse, ReleaseWorkspaceResponse, Repository,
        SnapshotWorkspaceResponse,
    };

    use super::*;

    /// A daemon whose heartbeats answer from a script; nothing else is
    /// called.
    #[derive(Default)]
    struct Fake {
        answers: Mutex<VecDeque<Result<RenewLeaseResponse, ClientError>>>,
    }

    fn not_here<T>() -> Result<T, ClientError> {
        Err(tonic::Status::unimplemented("not in this fake").into())
    }

    #[tonic::async_trait]
    impl WorkspacesApi for Fake {
        async fn status(&self) -> Result<GetDaemonStatusResponse, ClientError> {
            not_here()
        }
        async fn register_repository(
            &self,
            _: &Path,
            _: IfNotJj,
        ) -> Result<RegisterRepositoryResponse, ClientError> {
            not_here()
        }
        async fn list_repositories(&self) -> Result<Vec<Repository>, ClientError> {
            not_here()
        }
        async fn forget_repository(
            &self,
            _: &str,
        ) -> Result<ForgetRepositoryResponse, ClientError> {
            not_here()
        }
        async fn create_workspace(
            &self,
            _: CreateWorkspaceRequest,
        ) -> Result<CreateWorkspaceResponse, ClientError> {
            not_here()
        }
        async fn list_workspaces(
            &self,
            _: ListWorkspacesRequest,
        ) -> Result<Vec<Workspace>, ClientError> {
            not_here()
        }
        async fn inspect_workspace(
            &self,
            _: &str,
            _: &str,
            _: bool,
        ) -> Result<GetWorkspaceResponse, ClientError> {
            not_here()
        }
        async fn release_workspace(
            &self,
            _: &str,
            _: &str,
            _: bool,
            _: bool,
        ) -> Result<ReleaseWorkspaceResponse, ClientError> {
            not_here()
        }
        async fn snapshot_workspace(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<SnapshotWorkspaceResponse, ClientError> {
            not_here()
        }
        async fn renew_lease(
            &self,
            _: &str,
            workspace: &str,
            lease_id: &str,
        ) -> Result<RenewLeaseResponse, ClientError> {
            assert_eq!((workspace, lease_id), ("ws", "lease"));
            self.answers.lock().unwrap().pop_front().unwrap()
        }
    }

    fn down() -> Result<RenewLeaseResponse, ClientError> {
        Err(tonic::Status::unavailable("daemon is down").into())
    }

    fn renewed(every: Duration) -> RenewLeaseResponse {
        RenewLeaseResponse {
            lease: Some(Lease {
                id: "lease".into(),
                expires_at: Some((SystemTime::now() + Duration::from_secs(90)).into()),
                heartbeat_every: Some(every.try_into().unwrap()),
                ..Lease::default()
            }),
        }
    }

    /// A heartbeater over `answers` whose lease expires at `expires_at`,
    /// and the notes it makes.
    fn heartbeater(
        answers: Vec<Result<RenewLeaseResponse, ClientError>>,
        expires_at: SystemTime,
    ) -> (Heartbeater, Arc<Mutex<Vec<String>>>) {
        let fake = Fake {
            answers: Mutex::new(answers.into()),
        };
        let ws = Workspace {
            id: "ws".into(),
            ..Workspace::default()
        };
        let lease = Lease {
            id: "lease".into(),
            expires_at: Some(expires_at.into()),
            grace: Some(Duration::from_secs(1).try_into().unwrap()),
            heartbeat_every: Some(Duration::from_secs(10).try_into().unwrap()),
            ..Lease::default()
        };
        let notes = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&notes);
        let notify = Box::new(move |text: &str| sink.lock().unwrap().push(text.to_owned()));
        let heartbeater = Heartbeater::new(Arc::new(fake), &ws, &lease, notify).unwrap();
        (heartbeater, notes)
    }

    // 13
    #[tokio::test]
    async fn heartbeater_backoff() {
        let mut answers: Vec<_> = (0..7).map(|_| down()).collect();
        answers.push(Ok(renewed(Duration::from_secs(7))));
        let later = SystemTime::now() + Duration::from_secs(3600);
        let (mut heartbeater, notes) = heartbeater(answers, later);
        assert_eq!(heartbeater.next_delay(), Duration::from_secs(10));
        let mut delays = Vec::new();
        for _ in 0..7 {
            assert!(heartbeater.tick().await.unwrap().is_none());
            delays.push(heartbeater.next_delay().as_secs());
        }
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(notes.lock().unwrap().len(), 1);
        assert!(notes.lock().unwrap()[0].starts_with("reef: daemon unreachable ("));
        // A success resets to the daemon's interval, the new one it gave.
        assert!(heartbeater.tick().await.unwrap().is_some());
        assert_eq!(heartbeater.next_delay(), Duration::from_secs(7));
        let notes = notes.lock().unwrap();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert_eq!(notes[1], "reef: daemon reachable again; lease renewed");
    }

    #[tokio::test]
    async fn heartbeater_warns_once_when_the_lease_lapses() {
        let past = SystemTime::now() - Duration::from_secs(90);
        let (mut heartbeater, notes) = heartbeater(vec![down(), down()], past);
        heartbeater.tick().await.unwrap();
        heartbeater.tick().await.unwrap();
        let notes = notes.lock().unwrap();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[1].contains("lease lease has expired"), "{notes:?}");
    }

    #[tokio::test]
    async fn heartbeater_retries_other_failures() {
        let cut_off = Err(tonic::Status::internal("h2 protocol error").into());
        let later = SystemTime::now() + Duration::from_secs(3600);
        let (mut heartbeater, notes) = heartbeater(vec![cut_off], later);
        assert!(heartbeater.tick().await.unwrap().is_none());
        assert_eq!(heartbeater.next_delay(), Duration::from_secs(1));
        assert_eq!(notes.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn heartbeater_returns_refusals() {
        let refused = Err(tonic::Status::failed_precondition("lease ended").into());
        let later = SystemTime::now() + Duration::from_secs(3600);
        let (mut heartbeater, notes) = heartbeater(vec![refused], later);
        let error = heartbeater.tick().await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Conflict);
        assert!(notes.lock().unwrap().is_empty());
    }
}
