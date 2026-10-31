//! Launch files (R2.18, ADR 0047): how `reef ui` hands a browser a session
//! without putting its token on a command line. The daemon writes
//! `<state>/ui/open-<16 hex>.html` (`0600`, in a `0700` directory), a page
//! with no script that refreshes to the session URL; the CLI opens the
//! file's path. It is deleted at the session's first request, after
//! [`Limits::launch_file_ttl`](super::Limits::launch_file_ttl), or at the
//! next start.

use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use rand::RngExt as _;

use crate::daemon::ensure_private_dir;

/// Writes a launch file for `session_url` in `dir`, creating `dir` private.
pub(crate) fn write(dir: &Path, session_url: &str) -> io::Result<PathBuf> {
    ensure_private_dir(dir, true).map_err(io::Error::other)?;
    let name = format!("open-{:016x}.html", rand::rng().random::<u64>());
    let path = dir.join(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let url = escape(session_url);
    write!(
        file,
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n\
         <meta http-equiv=\"refresh\" content=\"0; url={url}\">\n<title>Reef</title>\n</head>\n\
         <body><p><a href=\"{url}\">Open the Reef web UI</a></p></body>\n</html>\n"
    )?;
    Ok(path)
}

/// `text` escaped for an HTML attribute. The URL is the daemon's own
/// (`http://127.0.0.1:<port>/#session=<base64url>`), so nothing needs it;
/// this keeps the page sound if that ever changes.
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

    #[test]
    fn launch_files_are_private_and_scriptless() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("ui");
        let url = "http://127.0.0.1:5/#session=abc";
        let path = write(&dir, url).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        let html = std::fs::read_to_string(&path).unwrap();
        assert!(html.contains(&format!("url={url}\"")));
        assert!(!html.contains("<script"));
        assert_eq!(sweep(&dir), 1);
        assert!(!path.exists());
    }
}
