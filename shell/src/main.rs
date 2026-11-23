#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
mod notify;
mod window;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use reef_client::paths::{ClientConfig, PathOverrides};
use reef_shell::login::{self, Act, Choice, Shown};
use reef_shell::menu::{MenuAction, MenuItem, MenuSpec};
use reef_shell::notice::Note;
use reef_shell::{Command, Page, Platform, Shell, ShellConfig, ShellHandle, link};
use tauri::menu::{CheckMenuItem, Menu, MenuItem as TauriItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, RunEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as _};
use tauri_plugin_deep_link::DeepLinkExt as _;

const TRAY: &str = "reef";

static SHELL: OnceLock<ShellHandle> = OnceLock::new();

fn shell() -> Option<&'static ShellHandle> {
    SHELL.get()
}

#[derive(Clone)]
struct App {
    handle: AppHandle,
}

impl Platform for App {
    fn show(&self, page: Page) {
        let handle = self.handle.clone();
        let _ = self
            .handle
            .run_on_main_thread(move || window::show(&handle, page));
    }

    fn post(&self, note: &Note) {
        #[cfg(target_os = "macos")]
        notify::post(note);
        #[cfg(not(target_os = "macos"))]
        tracing::info!(title = %note.title, "a notification (not posted on this platform yet)");
    }

    fn retract(&self, ids: &[String]) {
        #[cfg(target_os = "macos")]
        notify::retract(ids);
        #[cfg(not(target_os = "macos"))]
        let _ = ids;
    }

    fn render(&self, menu: &MenuSpec) {
        let app = self.clone();
        let menu = menu.clone();
        let _ = self.handle.run_on_main_thread(move || app.draw(&menu));
    }

    fn open_in_browser(&self, launch_file: &Path) {
        open(launch_file.as_os_str());
    }
}

impl App {
    fn draw(&self, spec: &MenuSpec) {
        let Some(tray) = self.handle.tray_by_id(TRAY) else {
            return;
        };
        let menu = match Menu::new(&self.handle) {
            Ok(menu) => menu,
            Err(error) => return tracing::warn!(%error, "no menu"),
        };
        for item in &spec.items {
            let added = match item {
                MenuItem::Label(label) => TauriItem::new(&self.handle, label, false, None::<&str>)
                    .and_then(|item| menu.append(&item)),
                MenuItem::Action { label, action } => {
                    TauriItem::with_id(&self.handle, action.id(), label, true, None::<&str>)
                        .and_then(|item| menu.append(&item))
                }
                MenuItem::Check { label, on, action } => CheckMenuItem::with_id(
                    &self.handle,
                    action.id(),
                    label,
                    true,
                    *on,
                    None::<&str>,
                )
                .and_then(|item| menu.append(&item)),
                MenuItem::Separator => {
                    PredefinedMenuItem::separator(&self.handle).and_then(|item| menu.append(&item))
                }
            };
            if let Err(error) = added {
                tracing::warn!(%error, "a menu item was not added");
            }
        }
        let _ = tray.set_menu(Some(menu));
        let _ = tray.set_title(Some(spec.title.as_str()));
        let tip = if spec.needs_you {
            format!("Reef: {} need you", spec.title)
        } else {
            "Reef".to_owned()
        };
        let _ = tray.set_tooltip(Some(tip));
    }

    fn clicked(&self, id: &str) {
        let (Some(action), Some(shell)) = (MenuAction::from_id(id), shell()) else {
            return;
        };
        match action {
            MenuAction::Open(path) => shell.send(Command::Open(path)),
            MenuAction::StartDaemon => shell.send(Command::StartDaemon),
            MenuAction::RestartDaemon => shell.send(Command::RestartDaemon),
            MenuAction::NotificationSettings => {
                let identifier = self.handle.config().identifier.clone();
                open(format!("x-apple.systempreferences:com.apple.Notifications-Settings.extension?id={identifier}").as_ref());
            }
            MenuAction::OpenAtLogin(on) => {
                let choice = if on { Choice::On } else { Choice::Off };
                shell.send(Command::OpenAtLogin(open_at_login(
                    &self.handle,
                    Some(choice),
                )));
            }
            MenuAction::Quit => self.handle.exit(0),
        }
    }
}

fn open(what: &std::ffi::OsStr) {
    if let Err(error) = std::process::Command::new("/usr/bin/open")
        .arg(what)
        .spawn()
    {
        tracing::warn!(%error, "open failed");
    }
}

/// Saves the person's `chosen` choice, if they made one, applies the saved
/// choice to the login item, and says what the menu shows for it.
fn open_at_login(handle: &AppHandle, chosen: Option<Choice>) -> Shown {
    let Ok(data_dir) = handle.path().app_data_dir() else {
        return Shown::Elsewhere;
    };
    let file = data_dir.join(login::CHOICE_FILE);
    let choice = match chosen {
        Some(choice) => {
            if let Err(error) = choice.save(&file) {
                tracing::warn!(%error, "the open at login choice was not saved");
            }
            choice
        }
        None => Choice::load(&file),
    };
    // The autostart plugin writes the canonical path, and its `is_enabled`
    // says only that its plist exists, wherever it points.
    let exe = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .ok();
    let plist = handle.path().home_dir().ok().map(|home| {
        home.join("Library/LaunchAgents")
            .join(format!("{}.plist", handle.package_info().name))
    });
    let agent = || {
        plist
            .as_ref()
            .and_then(|plist| std::fs::read_to_string(plist).ok())
    };
    let development = cfg!(debug_assertions);
    let launcher = handle.autolaunch();
    let set = match login::act(choice, exe.as_deref(), development, agent().as_deref()) {
        Act::Enable => launcher.enable(),
        Act::Disable => launcher.disable(),
        Act::Leave => Ok(()),
    };
    if let Err(error) = set {
        tracing::warn!(%error, "open at login did not change");
    }
    login::shown(exe.as_deref(), development, agent().as_deref())
}

fn bundled() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_owned();
    dir.join("reef").is_file().then_some(dir)
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Accessory);
    let handle = app.handle().clone();
    let data_dir = app.path().app_data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    let paths = ClientConfig::load(&PathOverrides::default())?;
    let home = app.path().home_dir()?;
    let platform = App {
        handle: handle.clone(),
    };
    let tray_app = platform.clone();
    TrayIconBuilder::with_id(TRAY)
        .icon(app.default_window_icon().cloned().ok_or("no icon")?)
        .show_menu_on_left_click(true)
        .on_menu_event(move |_, event| tray_app.clicked(event.id().as_ref()))
        .on_tray_icon_event(|_, event| {
            // The person may have changed it in System Settings since; the
            // pointer enters before the click opens the menu.
            #[cfg(target_os = "macos")]
            if matches!(
                event,
                tauri::tray::TrayIconEvent::Enter { .. } | tauri::tray::TrayIconEvent::Click { .. }
            ) {
                notify::refresh();
            }
            #[cfg(not(target_os = "macos"))]
            let _ = event;
        })
        .build(app)?;
    let config = ShellConfig {
        socket: paths.socket,
        data_dir,
        installed: link::installed_dirs(&home),
        bundled: bundled(),
    };
    let shell = tauri::async_runtime::block_on(async move { Shell::start(platform, config) });
    let _ = SHELL.set(shell.clone());

    #[cfg(target_os = "macos")]
    notify::init(shell.clone());

    shell.send(Command::OpenAtLogin(open_at_login(&handle, None)));

    let links = shell.clone();
    app.deep_link().on_open_url(move |event| {
        for url in event.urls() {
            links.send(Command::Link(url.to_string()));
        }
    });
    if let Ok(Some(urls)) = app.deep_link().get_current() {
        for url in urls {
            shell.send(Command::Link(url.to_string()));
        }
    }
    Ok(())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("REEF_LOG"))
        .init();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(setup)
        .build(tauri::generate_context!())
        .expect("Reef starts");
    app.run(|handle, event| match event {
        // Tauri requests an exit with no `code` when the last window closes, and with
        // one from `AppHandle::exit`, which Quit calls.
        RunEvent::ExitRequested {
            api, code: None, ..
        } => api.prevent_exit(),
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => {
            if let Some(shell) = shell() {
                shell.send(Command::Open(reef_client::web::WebPath::start()));
            }
        }
        _ => {
            let _ = handle;
        }
    });
}
