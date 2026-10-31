//! `reef ui` (R2.18, ADR 0047): opens the web UI in a browser on a new
//! session, without putting the session's token on any command line.
//!
//! The daemon writes a launch file only the user can read, a page that
//! refreshes to the session URL, and this command opens the file's path:
//! `$BROWSER <path>`, else `open` on macOS and `xdg-open` on Linux. The
//! token travels in the URL's fragment, which the app strips on load.
//!
//! It opens a browser only when stdout is a terminal (or with `--open`): a
//! command an agent runs must not pop a window on the user's desktop.
//! Elsewhere, and where no browser can be opened (over SSH, or on Linux with
//! no display), it prints the launch file's path, and `--print` asks for no
//! launch file and prints the session URL itself.

use std::ffi::OsString;
use std::io::{self, IsTerminal as _, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use reef_client::json::WebSession;
use serde::Serialize;

use super::out::{Output, TextRender, block};
use super::{CliError, Ctx};

/// How long the opener may take before `reef ui` stops waiting for it.
const OPENER_WAIT: Duration = Duration::from_secs(5);

/// `reef ui`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Print the session URL (a credential) instead of opening a browser.
    #[arg(long, conflicts_with = "open")]
    print: bool,
    /// Open a browser even when stdout is not a terminal.
    #[arg(long)]
    open: bool,
}

/// The text form: what was opened, or the URL to open.
#[derive(Serialize)]
struct Shown<'a> {
    #[serde(flatten)]
    session: &'a WebSession,
}

impl TextRender for Shown<'_> {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        match (&self.session.session_url, self.session.opened) {
            (_, true) => writeln!(w, "Opened {} in your browser.", self.session.url),
            (Some(session_url), false) => writeln!(w, "{session_url}"),
            (None, false) => {
                let mut fields = vec![("url", self.session.url.clone())];
                if let Some(path) = &self.session.launch_path {
                    fields.push(("launch_path", path.clone()));
                }
                block(w, &fields)
            }
        }
    }
}

/// Why no browser can be opened here, if none can: over SSH, or on Linux
/// with no display. `$BROWSER` overrides both.
fn no_browser_here() -> Option<&'static str> {
    if std::env::var_os("BROWSER").is_some_and(|b| !b.is_empty()) {
        return None;
    }
    if std::env::var_os("SSH_CONNECTION").is_some() {
        return Some("ssh");
    }
    if cfg!(target_os = "linux")
        && std::env::var_os("DISPLAY").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_none()
    {
        return Some("no display");
    }
    None
}

/// The opener for `path`: `$BROWSER`, else the platform's.
fn opener(path: &Path) -> Command {
    let program: OsString = std::env::var_os("BROWSER")
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "open".into()
            } else {
                "xdg-open".into()
            }
        });
    let mut command = Command::new(program);
    command
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Runs the opener on `path`; whether it exited zero within
/// [`OPENER_WAIT`] (one still running then is taken as having opened).
fn open(path: &Path) -> bool {
    let Ok(mut child) = opener(path).spawn() else {
        return false;
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() < OPENER_WAIT => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => return true,
            Err(_) => return false,
        }
    }
}

/// The port of `url` (`http://127.0.0.1:<port>/`).
fn port_of(url: &str) -> &str {
    url.trim_end_matches('/')
        .rsplit(':')
        .next()
        .unwrap_or_default()
}

pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    // Opening follows stdout being a terminal, whatever the output format.
    let wants_open = !args.print && (args.open || io::stdout().is_terminal());
    let blocked = if wants_open { no_browser_here() } else { None };
    // Every session but `--print`'s gets a launch file, so a caller that
    // opens nothing can still hand its path to the user.
    let response = ctx
        .client
        .create_web_session(!args.print)
        .await
        .map_err(CliError::Daemon)?;
    let launch_path = (!response.launch_path.is_empty()).then(|| response.launch_path.clone());
    let opened = match &launch_path {
        Some(path) if wants_open && blocked.is_none() => {
            let path = Path::new(path).to_owned();
            tokio::task::spawn_blocking(move || open(&path))
                .await
                .unwrap_or(false)
        }
        _ => false,
    };
    // Show the credential when asked, or when a browser was wanted and
    // could not be opened, on a terminal only.
    let fallback = wants_open && !opened && ctx.out.mode() == Output::Text;
    let session = WebSession {
        url: response.url.clone(),
        launch_path,
        opened,
        session_url: (args.print || fallback).then(|| response.session_url.clone()),
    };
    if fallback {
        let port = port_of(&response.url);
        ctx.out.note(&match blocked {
            Some("ssh") => format!(
                "This URL is a credential: anyone with it can read your repositories until the daemon stops. Over SSH, forward the port first: ssh -L {port}:127.0.0.1:{port} <host>"
            ),
            _ => "No browser could be opened. This URL is a credential: anyone with it can read your repositories until the daemon stops.".to_owned(),
        });
    } else if args.print && ctx.out.mode() == Output::Text {
        ctx.out.note(
            "This URL is a credential: anyone with it can read your repositories until the daemon stops.",
        );
    }
    ctx.out.object(&Shown { session: &session })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports() {
        assert_eq!(port_of("http://127.0.0.1:52817/"), "52817");
        assert_eq!(port_of("http://[::1]:7878/"), "7878");
    }
}
