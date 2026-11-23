//! Launch files (R2.18, ADR 0047): how `reef ui` hands a browser a session
//! without putting its token on a command line. The daemon writes
//! `<state>/ui/open-<16 hex>.html` (`0600`, in a `0700` directory), a page
//! that goes to the session URL; the CLI opens the file's path. It is
//! deleted at the session's first request, after
//! [`Limits::launch_file_ttl`](super::Limits::launch_file_ttl), or at the
//! next start.
//!
//! With a fixed port the page clears the address first, and goes nowhere
//! unless that succeeds ([`clear`](super::clear), #346). With a kernel-chosen
//! one it has no script and refreshes.

use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use rand::RngExt as _;

use crate::daemon::ensure_private_dir;

/// What a launch file opens.
#[derive(Debug)]
pub(crate) struct Opens {
    /// The session URL.
    pub(crate) session_url: String,
    /// The web UI's origin and the reset of it to ask for first.
    pub(crate) reset: Option<Reset>,
}

/// A reset a launch file asks for.
#[derive(Debug)]
pub(crate) struct Reset {
    /// `http://localhost:<port>`.
    pub(crate) origin: String,
    /// The reset's id.
    pub(crate) id: String,
}

/// Writes a launch file in `dir`, creating `dir` private.
pub(crate) fn write(dir: &Path, opens: &Opens) -> io::Result<PathBuf> {
    ensure_private_dir(dir, true).map_err(io::Error::other)?;
    let name = format!("open-{:016x}.html", rand::rng().random::<u64>());
    let path = dir.join(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(page(opens).as_bytes())?;
    Ok(path)
}

/// The file's page.
fn page(opens: &Opens) -> String {
    let url = escape(&opens.session_url);
    let (head, body) = match &opens.reset {
        None => (
            format!("<meta http-equiv=\"refresh\" content=\"0; url={url}\">\n"),
            format!("<p><a href=\"{url}\">Open the Reef web UI</a></p>"),
        ),
        Some(Reset { origin, id }) => {
            let origin = escape(origin);
            (
                String::new(),
                format!(
                    "<main data-url=\"{url}\" data-reset=\"{origin}{}{id}\">\n\
                     <p id=\"opening\">Opening Reef. This first clears what this browser kept for {origin}, which can take a moment.</p>\n\
                     <div id=\"failed\" hidden>\n\
                     <p>Reef was not opened: this browser could not be told to clear what it kept for {origin}.</p>\n\
                     <p>Another process may have left a page or cookies there. Clear this browser's data for {origin}, check <code>reef daemon status</code>, and run <code>reef ui</code> again.</p>\n\
                     </div>\n</main>\n\
                     <noscript>Opening Reef needs JavaScript.</noscript>\n\
                     <script>{SCRIPT}</script>",
                    super::clear::PATH
                ),
            )
        }
    };
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n{head}<title>Reef</title>\n</head>\n\
         <body>{body}</body>\n</html>\n"
    )
}

/// Goes to the session URL once the reset's image has loaded, and nowhere
/// otherwise. There is no time limit: a large cache takes a while to clear,
/// and giving up early would open an address that is not cleared.
const SCRIPT: &str = "\n\
    const { url, reset } = document.querySelector(\"main\").dataset;\n\
    const image = new Image();\n\
    image.onload = () => location.replace(url);\n\
    image.onerror = () => {\n  \
      document.getElementById(\"opening\").hidden = true;\n  \
      document.getElementById(\"failed\").hidden = false;\n\
    };\n\
    image.src = reset;\n";

/// `text` escaped for an HTML attribute. The URL is the daemon's own
/// (`http://localhost:<port>/?launch=<hex>#session=<base64url>`), so nothing
/// needs it; this keeps the page sound if that ever changes.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Deletes `path`, logging any failure but a missing file.
pub(crate) fn remove(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(path = %path.display(), %error, "cannot remove a launch file"),
    }
}

/// Deletes every `open-*.html` in `dir`, which an earlier run left; returns
/// how many.
pub(crate) fn sweep(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("open-") && name.ends_with(".html") {
            remove(&entry.path());
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    const URL: &str = "http://localhost:5/#session=abc";

    #[test]
    fn launch_files_are_private_and_scriptless() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("ui");
        let opens = Opens {
            session_url: URL.to_owned(),
            reset: None,
        };
        let path = write(&dir, &opens).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        let html = std::fs::read_to_string(&path).unwrap();
        assert!(html.contains(&format!("url={URL}\"")));
        assert!(!html.contains("<script"));
        assert_eq!(sweep(&dir), 1);
        assert!(!path.exists());
    }

    #[test]
    fn a_fixed_port_is_cleared_before_anything_opens_it() {
        let html = page(&Opens {
            session_url: URL.to_owned(),
            reset: Some(Reset {
                origin: "http://localhost:5".to_owned(),
                id: "ab".repeat(16),
            }),
        });
        let reset = format!(
            "data-reset=\"http://localhost:5/reset/{}\"",
            "ab".repeat(16)
        );
        assert!(html.contains(&reset), "{html}");
        assert!(html.contains(&format!("data-url=\"{URL}\"")), "{html}");
        // Nothing but the script goes on: no refresh, and no link to follow.
        assert!(
            !html.contains("http-equiv") && !html.contains("<a "),
            "{html}"
        );
    }
}
