//! Reef's desktop shell, without its toolkit (R3.39, ADR 0093).
//!
//! [`Shell::start`] runs one task that owns everything the shell knows: the
//! connection to the daemon, the window's session, the needs-you-now items
//! the menu lists, and which items it has announced. The app (`shell/`, on
//! Tauri) sends it [`Command`]s and implements [`Platform`]; nothing is
//! shared, so nothing locks. The task finds the daemon on its socket, starts
//! one through `reef daemon start` at launch when none answers, follows
//! `InboxService.WatchInbox`, and turns the stream into banners and a menu.
//!
//! The web UI in the window is the daemon's own page: a session minted over
//! the socket, never printed. Passkey ceremonies, which the webview cannot
//! run, go to the person's browser through a launch file, as `reef ui` does.

pub mod link;
pub mod login;
pub mod menu;
pub mod notice;

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reef_api::v1;
use reef_client::Client;
use reef_client::web::WebPath;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, sleep_until};

use crate::link::DeepLink;
use crate::menu::{Conn, Daemon, Listed, MenuSpec, View};
use crate::notice::{Note, Notifier, Step};

/// The oldest daemon the shell works with: the first with the raise ledger.
pub const API_FLOOR: u32 = 46;
const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);
const TOO_OLD_POLL: Duration = Duration::from_secs(60);
const STATUS_EVERY: Duration = Duration::from_secs(15);
const START_WAIT: Duration = Duration::from_secs(10);

/// Where the shell finds things.
#[derive(Clone, Debug)]
pub struct ShellConfig {
    /// The daemon's socket (`reef_client::paths`).
    pub socket: PathBuf,
    /// The app's own data directory: `notices.json` lives here.
    pub data_dir: PathBuf,
    /// Where an installed `reef` may be ([`link::installed_dirs`]).
    pub installed: Vec<PathBuf>,
    /// The directory of the app's own `reef`; `None` outside a bundle.
    pub bundled: Option<PathBuf>,
}

/// What the app tells the shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Show the window at a page (a menu row, a banner's click, Open Reef).
    Open(WebPath),
    /// The operating system opened a `reef://` link.
    Link(String),
    /// The window now shows `url`, so a new session can return there.
    Visited(String),
    /// The page asked for a new window at `url`: the passkey hand-off.
    NewWindow(String),
    /// The window was closed and its webview destroyed.
    WindowClosed,
    /// The menu's Start the daemon.
    StartDaemon,
    /// The menu's Restart the daemon (one too old, with no active
    /// workspace).
    RestartDaemon,
    /// Whether the person allows notifications.
    Notifications(bool),
    /// Whether the app opens at login.
    OpenAtLogin(login::Shown),
}

/// A session URL. A credential: it prints as its origin only.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionUrl(String);

impl SessionUrl {
    /// The whole URL, for the webview and nothing else.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// `http://localhost:<port>`, which the window is pinned to.
    pub fn origin(&self) -> &str {
        origin(&self.0)
    }
}

impl fmt::Debug for SessionUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionUrl({}/…)", self.origin())
    }
}

/// What the window shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Page {
    /// A new session: the window loads it and stays on its origin.
    Session(SessionUrl),
    /// Another page of the session it has.
    Navigate(String),
    /// The daemon is not reachable; the words say why.
    Offline(String),
}

/// What the app does for the shell. Every call returns at once; failures
/// are the app's to log.
pub trait Platform: Send + Sync + 'static {
    /// Shows the window at `page`, creating its webview if it has none.
    fn show(&self, page: Page);
    /// Posts a banner; posting its identifier again replaces it.
    fn post(&self, note: &Note);
    /// Takes banners back.
    fn retract(&self, ids: &[String]);
    /// Redraws the tray; called only when the spec changed.
    fn render(&self, menu: &MenuSpec);
    /// Opens a 0600 launch file in the default browser.
    fn open_in_browser(&self, launch_file: &Path);
}

/// The app's way into the shell's task.
#[derive(Clone, Debug)]
pub struct ShellHandle {
    tx: mpsc::UnboundedSender<Command>,
}

impl ShellHandle {
    /// Sends `command`; once the task has ended, nothing happens.
    pub fn send(&self, command: Command) {
        let _ = self.tx.send(command);
    }
}

/// The shell's task.
pub struct Shell;

impl Shell {
    /// Starts the task on the current Tokio runtime.
    pub fn start(platform: impl Platform, config: ShellConfig) -> ShellHandle {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run(platform, config, rx));
        ShellHandle { tx }
    }
}

fn origin(url: &str) -> &str {
    let after = url.find("://").map_or(0, |at| at + 3);
    let end = url[after..].find('/').map_or(url.len(), |at| after + at);
    &url[..end]
}

fn page_of(at: &str, url: &str) -> Option<WebPath> {
    if origin(url) != at {
        return None;
    }
    let rest = &url[at.len()..];
    WebPath::parse(rest.split(['?', '#']).next().unwrap_or_default())
}

fn name_of(root: &str) -> String {
    Path::new(root).file_name().map_or_else(
        || root.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The menu's needs-you-now items, per repository, as the stream says.
#[derive(Default)]
struct Held {
    repos: BTreeMap<String, HeldRepo>,
    /// Repositories this stream has sent, and those whose listing began.
    seen: HashSet<String>,
    listing: HashSet<String>,
    synced: bool,
}

#[derive(Default)]
struct HeldRepo {
    total: u32,
    items: BTreeMap<String, (i64, String)>,
}

impl Held {
    fn new_stream(&mut self) {
        self.seen.clear();
        self.listing.clear();
        self.synced = false;
    }

    /// Folds `frame`; a new stream's listing of a repository replaces what
    /// the last stream left, and one it never sends by `synced` is gone.
    fn fold(&mut self, frame: &v1::WatchInboxResponse) {
        for section in &frame.sections {
            self.fold_section(section);
        }
        if frame.synced && !self.synced {
            self.synced = true;
            self.repos.retain(|repo, _| self.seen.contains(repo));
        }
    }

    fn fold_section(&mut self, section: &v1::InboxSectionChange) {
        self.seen.insert(section.repo_id.clone());
        if section.removed {
            self.repos.remove(&section.repo_id);
            return;
        }
        // Before its listing starts on this stream, a section is a failed
        // read with nothing to show: the last stream's items and total stand.
        if section.listing() == v1::InboxListing::Unspecified
            && !self.listing.contains(&section.repo_id)
            && self.repos.contains_key(&section.repo_id)
        {
            return;
        }
        let repo = self.repos.entry(section.repo_id.clone()).or_default();
        if section.listing() != v1::InboxListing::Unspecified
            && self.listing.insert(section.repo_id.clone())
        {
            repo.items.clear();
        }
        for key in &section.left {
            repo.items.remove(key);
        }
        for item in &section.items {
            if item.bucket() == v1::InboxBucket::NeedsYouNow {
                let since = item.since.as_ref().map_or(0, |at| at.seconds);
                repo.items
                    .insert(item.key.clone(), (since, item.headline.clone()));
            } else {
                repo.items.remove(&item.key);
            }
        }
        repo.total = section.needs_you_now_total;
    }

    fn total(&self) -> u32 {
        self.repos.values().map(|repo| repo.total).sum()
    }

    fn newest(&self, names: &BTreeMap<String, String>) -> Vec<Listed> {
        let mut all: Vec<(i64, &str, &str, &str)> = self
            .repos
            .iter()
            .flat_map(|(repo, held)| {
                held.items.iter().map(move |(key, (since, headline))| {
                    (*since, repo.as_str(), key.as_str(), headline.as_str())
                })
            })
            .collect();
        all.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.cmp(b.2)));
        all.into_iter()
            .take(menu::LISTED)
            .filter_map(|(_, repo, key, headline)| {
                Some(Listed {
                    headline: headline.to_owned(),
                    repo: names
                        .get(repo)
                        .cloned()
                        .unwrap_or_else(|| repo.chars().take(8).collect()),
                    open: WebPath::item(repo, key)?,
                })
            })
            .collect()
    }
}

struct State<P> {
    platform: P,
    config: ShellConfig,
    conn: Conn,
    client: Option<Client>,
    held: Held,
    names: BTreeMap<String, String>,
    notifier: Notifier,
    window: Option<Window>,
    visited: WebPath,
    notifications_off: bool,
    open_at_login: login::Shown,
    menu: Option<MenuSpec>,
    asked: Option<Ask>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ask {
    Start,
    Restart,
}

struct Window {
    session: Option<(u32, String)>,
}

impl<P: Platform> State<P> {
    fn view(&self) -> View {
        View {
            conn: self.conn.clone(),
            needs_total: self.held.total(),
            needs: self.held.newest(&self.names),
            notifications_off: self.notifications_off,
            open_at_login: self.open_at_login,
        }
    }

    fn redraw(&mut self) {
        let spec = menu::menu(&self.view());
        if self.menu.as_ref() != Some(&spec) {
            self.platform.render(&spec);
            self.menu = Some(spec);
        }
    }

    fn set_conn(&mut self, conn: Conn) {
        self.conn = conn;
        if !matches!(self.conn, Conn::Up(_))
            && let Some(window) = &mut self.window
        {
            window.session = None;
            self.platform.show(Page::Offline(self.offline_reason()));
        }
        self.redraw();
    }

    fn offline_reason(&self) -> String {
        match &self.conn {
            Conn::Down(why) => why.clone(),
            Conn::TooOld(daemon) => format!(
                "the daemon ({}) is older than this app needs",
                daemon.version
            ),
            _ => "connecting to the daemon".to_owned(),
        }
    }

    fn path_on_window(&self, url: &str) -> Option<WebPath> {
        let (_, at) = self.window.as_ref()?.session.as_ref()?;
        page_of(at, url)
    }

    fn run_step(&self, step: &Step) {
        for note in &step.post {
            self.platform.post(note);
        }
        if !step.retract.is_empty() {
            self.platform.retract(&step.retract);
        }
    }

    async fn open(&mut self, path: WebPath) {
        self.visited = path.clone();
        let shown = self
            .window
            .get_or_insert(Window { session: None })
            .session
            .clone();
        let (Conn::Up(daemon), Some(client)) = (&self.conn, &self.client) else {
            self.platform.show(Page::Offline(self.offline_reason()));
            return;
        };
        if let Some((pid, origin)) = shown
            && pid == daemon.pid
        {
            self.platform
                .show(Page::Navigate(format!("{origin}{path}")));
            return;
        }
        let pid = daemon.pid;
        match client.create_web_session(false, path.as_str()).await {
            Ok(session) => {
                let url = SessionUrl(format!("{}&embedded=1", session.session_url));
                self.window = Some(Window {
                    session: Some((pid, url.origin().to_owned())),
                });
                self.platform.show(Page::Session(url));
            }
            Err(error) => self
                .platform
                .show(Page::Offline(format!("the web UI did not open: {error}"))),
        }
    }

    async fn hand_off(&mut self, url: &str) {
        let (Some(path), Some(client)) = (self.path_on_window(url), &self.client) else {
            return;
        };
        match client.create_web_session(true, path.as_str()).await {
            Ok(session) if !session.launch_path.is_empty() => {
                self.platform
                    .open_in_browser(Path::new(&session.launch_path));
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "the browser hand-off did not open"),
        }
    }

    async fn command(&mut self, command: Command) {
        match command {
            Command::Open(path) => self.open(path).await,
            Command::Link(url) => {
                if let Some(DeepLink(path)) = DeepLink::parse(&url) {
                    self.open(path).await;
                } else {
                    tracing::warn!("ignored a link that is not reef://local/<page>");
                }
            }
            Command::Visited(url) => {
                if let Some(path) = self.path_on_window(&url) {
                    self.visited = path;
                }
            }
            Command::NewWindow(url) => self.hand_off(&url).await,
            Command::WindowClosed => self.window = None,
            Command::StartDaemon => self.asked = Some(Ask::Start),
            Command::RestartDaemon => self.asked = Some(Ask::Restart),
            Command::Notifications(on) => {
                self.notifications_off = !on;
                self.redraw();
            }
            Command::OpenAtLogin(on) => {
                self.open_at_login = on;
                // The toolkit flips a check item when clicked, so a change
                // that failed must draw the menu again.
                self.menu = None;
                self.redraw();
            }
        }
    }

    fn starter(&self) -> Option<PathBuf> {
        link::starter(
            &self.config.installed,
            self.config.bundled.as_deref(),
            Path::is_file,
        )
    }

    async fn daemon_command(&self, verbs: &[&str]) -> Result<(), String> {
        let Some(reef) = self.starter() else {
            return Err("no reef is installed".to_owned());
        };
        let output = tokio::process::Command::new(&reef)
            .args(verbs)
            .args(["-o", "json"])
            .env("REEF_SOCKET", &self.config.socket)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| format!("{}: {error}", reef.display()))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }

    async fn read_status(&self, client: &Client) -> Result<Daemon, String> {
        let status = client
            .daemon_status(true)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Daemon {
            version: status.version,
            api_revision: status.api_revision,
            pid: status.pid,
            repositories: status.repository_count,
            active_workspaces: status.active_workspace_count,
            pushes_attention: status.pushes_attention,
        })
    }

    async fn read_names(&mut self, client: &Client) {
        if let Ok(repos) = client.list_repositories().await {
            self.names = repos
                .into_iter()
                .map(|repo| (repo.id, name_of(&repo.root)))
                .collect();
        }
    }
}

async fn next_command(
    rx: &mut mpsc::UnboundedReceiver<Command>,
    until: Instant,
) -> Option<Command> {
    tokio::select! {
        command = rx.recv() => command,
        () = sleep_until(until) => None,
    }
}

async fn run<P: Platform>(
    platform: P,
    config: ShellConfig,
    mut rx: mpsc::UnboundedReceiver<Command>,
) {
    let notifier = Notifier::load(&config.data_dir.join("notices.json"));
    let mut state = State {
        platform,
        config,
        conn: Conn::Connecting,
        client: None,
        held: Held::default(),
        names: BTreeMap::new(),
        notifier,
        window: None,
        visited: WebPath::start(),
        notifications_off: false,
        open_at_login: login::Shown::Off,
        menu: None,
        asked: Some(Ask::Start),
    };
    state.redraw();
    let mut retry = FIRST_RETRY;
    loop {
        if rx.is_closed() {
            return;
        }
        let connected = match Client::connect(&state.config.socket).await {
            Ok(client) => state
                .read_status(&client)
                .await
                .map(|daemon| (client, daemon)),
            Err(error) => Err(error.to_string()),
        };
        let (client, daemon) = match connected {
            Ok(up) => up,
            Err(why) => {
                down(&mut state, &mut rx, why, &mut retry).await;
                continue;
            }
        };
        state.asked.take_if(|ask| *ask == Ask::Start);
        retry = FIRST_RETRY;
        if daemon.api_revision < API_FLOOR {
            too_old(&mut state, &mut rx, client, daemon).await;
            continue;
        }
        state.read_names(&client).await;
        state.client = Some(client.clone());
        let pid = daemon.pid;
        state.set_conn(Conn::Up(daemon));
        if state
            .window
            .as_ref()
            .is_some_and(|window| window.session.as_ref().is_none_or(|(at, _)| *at != pid))
        {
            state.open(state.visited.clone()).await;
        }
        let ended = follow(&mut state, &client, &mut rx).await;
        state.client = None;
        state.set_conn(Conn::Down(ended));
    }
}

async fn down<P: Platform>(
    state: &mut State<P>,
    rx: &mut mpsc::UnboundedReceiver<Command>,
    why: String,
    retry: &mut Duration,
) {
    state.client = None;
    if state.asked.take_if(|ask| *ask == Ask::Start).is_some() {
        state.set_conn(Conn::Starting);
        match state.daemon_command(&["daemon", "start"]).await {
            Ok(()) => {
                let deadline = Instant::now() + START_WAIT;
                while Client::connect(&state.config.socket).await.is_err()
                    && Instant::now() < deadline
                {
                    sleep(Duration::from_millis(200)).await;
                }
                return;
            }
            Err(error) => state.set_conn(Conn::Down(error)),
        }
    } else {
        state.set_conn(Conn::Down(why));
    }
    let until = Instant::now() + *retry;
    *retry = (*retry * 2).min(MAX_RETRY);
    while let Some(command) = next_command(rx, until).await {
        let start = command == Command::StartDaemon;
        state.command(command).await;
        if start {
            return;
        }
    }
}

async fn too_old<P: Platform>(
    state: &mut State<P>,
    rx: &mut mpsc::UnboundedReceiver<Command>,
    client: Client,
    daemon: Daemon,
) {
    let idle = daemon.active_workspaces == 0;
    state.client = Some(client);
    state.set_conn(Conn::TooOld(daemon));
    if idle && state.asked.take_if(|ask| *ask == Ask::Restart).is_some() {
        if let Err(error) = state.daemon_command(&["daemon", "stop"]).await {
            tracing::warn!(%error, "the daemon did not stop");
        }
        state.asked = Some(Ask::Start);
        return;
    }
    let until = Instant::now() + TOO_OLD_POLL;
    while let Some(command) = next_command(rx, until).await {
        let restart = command == Command::RestartDaemon;
        state.command(command).await;
        if restart {
            return;
        }
    }
}

async fn follow<P: Platform>(
    state: &mut State<P>,
    client: &Client,
    rx: &mut mpsc::UnboundedReceiver<Command>,
) -> String {
    let mut stream = match client.watch_inbox(v1::WatchInboxRequest::default()).await {
        Ok(stream) => stream,
        Err(error) => return error.to_string(),
    };
    state.held.new_stream();
    state.notifier.new_stream();
    let mut status_at = Instant::now();
    loop {
        let retract_at = state.notifier.deadline();
        tokio::select! {
            frame = stream.message() => {
                let frame = match frame {
                    Ok(Some(frame)) => frame,
                    Ok(None) => return "the daemon stopped".to_owned(),
                    Err(status) => return status.message().to_owned(),
                };
                if frame.sections.iter().any(|section| !section.removed && !state.names.contains_key(&section.repo_id)) {
                    state.read_names(client).await;
                }
                state.held.fold(&frame);
                let step = state.notifier.frame(&frame, &state.names, Instant::now());
                state.run_step(&step);
                state.notifier.commit();
                let keepalive = frame.sections.is_empty();
                if keepalive || status_at.elapsed() >= STATUS_EVERY {
                    status_at = Instant::now();
                    if let Ok(daemon) = state.read_status(client).await {
                        state.conn = Conn::Up(daemon);
                    }
                }
                state.redraw();
            }
            command = rx.recv() => {
                let Some(command) = command else {
                    return "the app quit".to_owned();
                };
                state.command(command).await;
            }
            () = async {
                match retract_at {
                    Some(at) => sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                let step = state.notifier.tick(Instant::now());
                state.run_step(&step);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_url_prints_its_origin_only() {
        let url = SessionUrl("http://localhost:52817/repos/r#session=secret&embedded=1".to_owned());
        assert_eq!(url.origin(), "http://localhost:52817");
        let printed = format!("{url:?}");
        assert!(!printed.contains("secret"), "{printed}");
    }

    #[test]
    fn a_page_is_on_the_windows_origin_only() {
        let at = "http://localhost:1";
        let page = |url| page_of(at, url).map(|path| path.as_str().to_owned());
        assert_eq!(
            page("http://localhost:1/repos/r/smartlog?change=c#x"),
            Some("/repos/r/smartlog".to_owned())
        );
        assert_eq!(
            page("http://localhost:1/repos/r?launch=0123456789abcdef#session=secret"),
            Some("/repos/r".to_owned())
        );
        assert_eq!(page("http://localhost:1"), Some(String::new()));
        assert_eq!(page("http://localhost:12/repos/r"), None, "another port");
        assert_eq!(page("http://evil.example/repos/r"), None);
        assert_eq!(page("http://localhost:1/a/../b"), None);
    }

    fn needs(repo: &str, key: &str) -> v1::InboxItem {
        v1::InboxItem {
            key: key.to_owned(),
            repo_id: repo.to_owned(),
            bucket: v1::InboxBucket::NeedsYouNow.into(),
            ..v1::InboxItem::default()
        }
    }

    fn listed(
        repo: &str,
        listing: v1::InboxListing,
        items: Vec<v1::InboxItem>,
    ) -> v1::InboxSectionChange {
        v1::InboxSectionChange {
            repo_id: repo.to_owned(),
            listing: listing.into(),
            needs_you_now_total: u32::try_from(items.len()).unwrap(),
            items,
            ..v1::InboxSectionChange::default()
        }
    }

    // R3.39 review R1.F5: a new stream's listing replaces what the last left,
    // even after a failed first section, and a repository it never sends is gone.
    #[test]
    fn a_new_stream_replaces_the_menus_items() {
        let mut held = Held::default();
        held.new_stream();
        held.fold(&v1::WatchInboxResponse {
            sections: vec![
                listed("a", v1::InboxListing::End, vec![needs("a", "old")]),
                listed("b", v1::InboxListing::End, vec![needs("b", "x")]),
            ],
            synced: true,
            ..v1::WatchInboxResponse::default()
        });
        assert_eq!(held.total(), 2);
        held.new_stream();
        held.fold(&v1::WatchInboxResponse {
            sections: vec![listed("a", v1::InboxListing::Unspecified, vec![])],
            ..v1::WatchInboxResponse::default()
        });
        assert_eq!(
            (held.total(), held.newest(&BTreeMap::new()).len()),
            (2, 2),
            "a failed first read keeps what the last stream showed"
        );
        held.fold(&v1::WatchInboxResponse {
            sections: vec![listed("a", v1::InboxListing::End, vec![])],
            synced: true,
            ..v1::WatchInboxResponse::default()
        });
        assert_eq!(held.total(), 0);
        assert_eq!(
            held.repos.keys().collect::<Vec<_>>(),
            ["a"],
            "b was forgotten while away"
        );
        assert!(held.repos["a"].items.is_empty());
    }

    #[test]
    fn names_are_a_roots_last_part() {
        assert_eq!(name_of("/Users/p/Projects/reef"), "reef");
        assert_eq!(origin("http://localhost:1/"), "http://localhost:1");
        assert_eq!(origin("http://localhost:1"), "http://localhost:1");
    }
}
