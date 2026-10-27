//! Helpers shared by `reef-vcs`'s integration tests.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "each test binary uses some of these helpers; a panic is the failure report"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use serde_json::Value;

/// This build's `reef-daemon`, which `reef` runs for `daemon run`, `config
/// show`, `restore` and autostart (R2.05). A test binary can only name its
/// own package's binaries, so it is looked for beside `reef`: a
/// workspace-wide build (`cargo nextest run --workspace`, `just test`)
/// puts it there. When it is missing (`-p reef-vcs`), or older than `reef`
/// and so may predate a change to the client both link, it is built once,
/// in the profile the tests are built in.
pub(crate) fn daemon_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let reef = Path::new(env!("CARGO_BIN_EXE_reef"));
        let beside = reef.with_file_name("reef-daemon");
        let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if beside.is_file() && modified(&beside) >= modified(reef) {
            return beside;
        }
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let output = Command::new(cargo)
            .args([
                "build",
                "-p",
                "reef-daemon",
                "--bin",
                "reef-daemon",
                "--message-format=json-render-diagnostics",
            ])
            .args((!cfg!(debug_assertions)).then_some("--release"))
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stderr(Stdio::inherit())
            .output()
            .unwrap();
        assert!(output.status.success(), "cargo build -p reef-daemon failed");
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|message| {
                message["reason"] == "compiler-artifact"
                    && message["target"]["name"] == "reef-daemon"
            })
            .and_then(|message| message["executable"].as_str().map(PathBuf::from))
            .expect("cargo reported no `reef-daemon` executable")
    })
}

/// Links this build's `reef-daemon` into `dir`, beside a copy of `reef`
/// there, so the daemon that copy starts is never another build's.
pub(crate) fn daemon_beside(dir: &Path) {
    std::os::unix::fs::symlink(daemon_bin(), dir.join("reef-daemon")).unwrap();
}
