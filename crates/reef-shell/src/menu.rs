//! The menu bar (R3.39): a pure function of what the shell knows, so the
//! tray is redrawn only when it differs.

use reef_client::web::WebPath;

/// How many needs-you-now items the menu lists.
pub const LISTED: usize = 5;

/// The daemon, as `GetDaemonStatus` last said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Daemon {
    /// Its version, such as `0.4.3`.
    pub version: String,
    /// The `reef.api.v1` revision it serves.
    pub api_revision: u32,
    /// Its process id: another means it restarted, and every web session
    /// ended.
    pub pid: u32,
    /// Registered repositories.
    pub repositories: u32,
    /// Active workspaces.
    pub active_workspaces: u32,
    /// Landing pushes to a git remote that need a person.
    pub pushes_attention: u32,
}

/// The connection to the daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conn {
    /// Trying the socket.
    Connecting,
    /// `reef daemon start` runs.
    Starting,
    /// Following its inbox.
    Up(Daemon),
    /// Older than the shell needs (`API_FLOOR`).
    TooOld(Daemon),
    /// Lost or never found; the words say why.
    Down(String),
}

/// A needs-you-now item as the menu lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    /// The daemon's headline.
    pub headline: String,
    /// The repository's name.
    pub repo: String,
    /// The item's page.
    pub open: WebPath,
}

/// What the menu shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// The connection.
    pub conn: Conn,
    /// What needs the person, over every repository.
    pub needs_total: u32,
    /// The newest needs-you-now items, newest first.
    pub needs: Vec<Listed>,
    /// The person turned notifications off for the app.
    pub notifications_off: bool,
    /// The app opens at login.
    pub open_at_login: bool,
}

/// What a menu item does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuAction {
    /// Show the window at a page.
    Open(WebPath),
    /// Start a daemon through `reef daemon start`.
    StartDaemon,
    /// Stop a daemon too old (with no active workspace), then start one.
    RestartDaemon,
    /// Open System Settings at the app's notifications.
    NotificationSettings,
    /// Open at login, or not.
    OpenAtLogin(bool),
    /// Quit the app; the daemon keeps running.
    Quit,
}

impl MenuAction {
    /// The menu item's id: the same action has the same id in every drawing
    /// of the menu, so a click on a menu drawn before the last change does
    /// what its row said.
    pub fn id(&self) -> String {
        match self {
            MenuAction::Open(path) => format!("open:{path}"),
            MenuAction::StartDaemon => "start".to_owned(),
            MenuAction::RestartDaemon => "restart".to_owned(),
            MenuAction::NotificationSettings => "settings".to_owned(),
            MenuAction::OpenAtLogin(on) => format!("login:{on}"),
            MenuAction::Quit => "quit".to_owned(),
        }
    }

    /// The action of a menu item's id.
    pub fn from_id(id: &str) -> Option<MenuAction> {
        Some(match id {
            "start" => MenuAction::StartDaemon,
            "restart" => MenuAction::RestartDaemon,
            "settings" => MenuAction::NotificationSettings,
            "login:true" => MenuAction::OpenAtLogin(true),
            "login:false" => MenuAction::OpenAtLogin(false),
            "quit" => MenuAction::Quit,
            _ => MenuAction::Open(WebPath::parse(id.strip_prefix("open:")?)?),
        })
    }
}

/// One menu item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuItem {
    /// Words, not clickable.
    Label(String),
    /// A clickable row.
    Action {
        /// Its words.
        label: String,
        /// What a click does.
        action: MenuAction,
    },
    /// A row with a check mark.
    Check {
        /// Its words.
        label: String,
        /// Whether it is checked.
        on: bool,
        /// What a click does.
        action: MenuAction,
    },
    /// A line between groups.
    Separator,
}

/// The tray: its title beside the icon, whether the icon says something
/// needs the person, and the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuSpec {
    /// Beside the icon: the needs-you-now count, or nothing.
    pub title: String,
    /// The icon says something needs the person.
    pub needs_you: bool,
    /// The menu, top to bottom.
    pub items: Vec<MenuItem>,
}

fn plural(n: u32, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

fn action(label: impl Into<String>, action: MenuAction) -> MenuItem {
    MenuItem::Action {
        label: label.into(),
        action,
    }
}

/// The tray for `view`.
pub fn menu(view: &View) -> MenuSpec {
    let mut items = Vec::new();
    let up = matches!(view.conn, Conn::Up(_));
    if up {
        items.push(MenuItem::Label(if view.needs_total == 0 {
            "Nothing needs you".to_owned()
        } else {
            format!("{} need you now", view.needs_total)
        }));
        for listed in view.needs.iter().take(LISTED) {
            items.push(action(
                format!("{} · {}", listed.headline, listed.repo),
                MenuAction::Open(listed.open.clone()),
            ));
        }
        let shown = u32::try_from(view.needs.len().min(LISTED)).unwrap_or(u32::MAX);
        if view.needs_total > shown {
            items.push(action(
                format!("and {} more", view.needs_total - shown),
                MenuAction::Open(WebPath::start()),
            ));
        }
        items.push(MenuItem::Separator);
    }
    match &view.conn {
        Conn::Connecting => items.push(MenuItem::Label("Connecting to the daemon".to_owned())),
        Conn::Starting => items.push(MenuItem::Label("Starting the daemon".to_owned())),
        Conn::Up(daemon) => {
            items.push(MenuItem::Label(format!(
                "{} active in {}",
                plural(daemon.active_workspaces, "workspace", "workspaces"),
                plural(daemon.repositories, "repository", "repositories"),
            )));
            if daemon.pushes_attention > 0 {
                items.push(MenuItem::Label(format!(
                    "{} to a git remote {} you",
                    plural(daemon.pushes_attention, "push", "pushes"),
                    if daemon.pushes_attention == 1 {
                        "needs"
                    } else {
                        "need"
                    },
                )));
            }
            items.push(MenuItem::Label(format!("Daemon {}", daemon.version)));
        }
        Conn::TooOld(daemon) => {
            items.push(MenuItem::Label(format!(
                "Daemon {} is older than this app needs",
                daemon.version
            )));
            if daemon.active_workspaces == 0 {
                items.push(action(
                    "Upgrade reef, then restart the daemon",
                    MenuAction::RestartDaemon,
                ));
            } else {
                items.push(MenuItem::Label(format!(
                    "Upgrade reef, then restart it once its {} end",
                    plural(daemon.active_workspaces, "workspace", "workspaces")
                )));
            }
        }
        Conn::Down(why) => {
            items.push(MenuItem::Label(format!("The daemon is not running: {why}")));
            items.push(action("Start the daemon", MenuAction::StartDaemon));
        }
    }
    if view.notifications_off {
        items.push(action(
            "Notifications are off: open Settings",
            MenuAction::NotificationSettings,
        ));
    }
    items.push(MenuItem::Separator);
    items.push(action("Open Reef", MenuAction::Open(WebPath::start())));
    items.push(MenuItem::Check {
        label: "Open at login".to_owned(),
        on: view.open_at_login,
        action: MenuAction::OpenAtLogin(!view.open_at_login),
    });
    items.push(action("Quit Reef (agents keep running)", MenuAction::Quit));
    MenuSpec {
        title: if up && view.needs_total > 0 {
            view.needs_total.to_string()
        } else {
            String::new()
        },
        needs_you: up && view.needs_total > 0,
        items,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon(active: u32) -> Daemon {
        Daemon {
            version: "0.4.3".to_owned(),
            api_revision: 46,
            pid: 1,
            repositories: 2,
            active_workspaces: active,
            pushes_attention: 0,
        }
    }

    fn view(conn: Conn, needs_total: u32, needs: usize) -> View {
        View {
            conn,
            needs_total,
            needs: (0..needs)
                .map(|n| Listed {
                    headline: format!("Asks for help {n}"),
                    repo: "reef".to_owned(),
                    open: WebPath::parse(&format!("/repos/r/items/help:r:{n}")).unwrap(),
                })
                .collect(),
            notifications_off: false,
            open_at_login: true,
        }
    }

    fn labels(spec: &MenuSpec) -> Vec<String> {
        spec.items
            .iter()
            .map(|item| match item {
                MenuItem::Label(label)
                | MenuItem::Action { label, .. }
                | MenuItem::Check { label, .. } => label.clone(),
                MenuItem::Separator => "-".to_owned(),
            })
            .collect()
    }

    #[test]
    fn a_daemon_that_needs_you() {
        let spec = menu(&view(Conn::Up(daemon(5)), 7, 6));
        assert_eq!(spec.title, "7");
        assert!(spec.needs_you);
        let labels = labels(&spec);
        assert_eq!(labels[0], "7 need you now");
        assert_eq!(labels[1], "Asks for help 0 · reef");
        assert_eq!(labels[6], "and 2 more");
        assert!(
            labels.contains(&"5 workspaces active in 2 repositories".to_owned()),
            "{labels:?}"
        );
        assert_eq!(
            labels.last().map(String::as_str),
            Some("Quit Reef (agents keep running)")
        );
    }

    #[test]
    fn every_action_keeps_its_id() {
        let open = MenuAction::Open(WebPath::parse("/repos/r/items/help:r:1").unwrap());
        for action in [
            open,
            MenuAction::Open(WebPath::start()),
            MenuAction::StartDaemon,
            MenuAction::RestartDaemon,
            MenuAction::NotificationSettings,
            MenuAction::OpenAtLogin(true),
            MenuAction::OpenAtLogin(false),
            MenuAction::Quit,
        ] {
            assert_eq!(MenuAction::from_id(&action.id()), Some(action));
        }
        assert_eq!(MenuAction::from_id("open:http://evil.example"), None);
        assert_eq!(MenuAction::from_id("3"), None);
    }

    #[test]
    fn nothing_needs_you() {
        let spec = menu(&view(Conn::Up(daemon(1)), 0, 0));
        assert_eq!(spec.title, "");
        assert!(!spec.needs_you);
        assert_eq!(labels(&spec)[0], "Nothing needs you");
        assert!(labels(&spec).contains(&"1 workspace active in 2 repositories".to_owned()));
    }

    #[test]
    fn a_daemon_too_old_restarts_only_when_idle() {
        let busy = menu(&view(Conn::TooOld(daemon(3)), 0, 0));
        assert!(
            labels(&busy)
                .contains(&"Upgrade reef, then restart it once its 3 workspaces end".to_owned())
        );
        assert!(!busy.items.iter().any(|item| matches!(
            item,
            MenuItem::Action {
                action: MenuAction::RestartDaemon,
                ..
            }
        )));
        let idle = menu(&view(Conn::TooOld(daemon(0)), 0, 0));
        assert!(idle.items.iter().any(|item| matches!(
            item,
            MenuItem::Action {
                action: MenuAction::RestartDaemon,
                ..
            }
        )));
    }

    #[test]
    fn a_daemon_that_is_down_offers_to_start() {
        let spec = menu(&view(Conn::Down("connection refused".to_owned()), 3, 0));
        assert_eq!(spec.title, "", "a count it cannot vouch for is not shown");
        assert!(spec.items.iter().any(|item| matches!(
            item,
            MenuItem::Action {
                action: MenuAction::StartDaemon,
                ..
            }
        )));
    }
}
