//! A trial's environment: cleared and rebuilt, so nothing from the machine
//! running the benchmark (a harness session's variables, a user's
//! configuration, a `reef` on `PATH`) reaches the agent. The variables kept
//! are in [`PASSED_THROUGH`].

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

/// Variables a trial keeps from the benchmark's environment. Everything
/// else is cleared.
pub const PASSED_THROUGH: [&str; 11] = [
    "ANTHROPIC_API_KEY",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
];

/// The cleared environment's base: home, state directory, a shell, and
/// [`PASSED_THROUGH`].
pub fn base_env(home: &Path, state: &Path) -> Vec<(String, OsString)> {
    let mut env: Vec<(String, OsString)> = PASSED_THROUGH
        .iter()
        .filter_map(|name| Some(((*name).to_owned(), std::env::var_os(name)?)))
        .collect();
    env.push(("HOME".to_owned(), home.into()));
    env.push(("REEF_STATE_DIR".to_owned(), state.into()));
    let shell = if Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "/bin/sh"
    };
    env.push(("SHELL".to_owned(), shell.into()));
    env
}

/// `env` without any credential ([`super::scrub::SECRET_VARIABLES`]): for
/// processes that are not the agent harness, such as a daemon or a scripted
/// command, which have no use for a model's key.
pub fn without_credentials(env: &mut Vec<(String, OsString)>) {
    env.retain(|(name, _)| !super::scrub::SECRET_VARIABLES.contains(&name.as_str()));
}

/// `inherited` (the benchmark's `PATH`) with no `reef` on it: the build
/// under test's directory is left out, and any other directory holding a
/// `reef` or `reef-daemon` is replaced by a directory under `shadow_root`
/// of symlinks to everything else in it, so the agent keeps the rest of
/// what is there (`~/.cargo/bin`, `/opt/homebrew/bin`). `bin` (the trial's
/// copy of the build) comes first when the agent should have Reef.
pub fn agent_path(
    inherited: &OsStr,
    with_reef: bool,
    bin: &Path,
    reef_bin_dir: &Path,
    shadow_root: &Path,
) -> Result<OsString, String> {
    let mut dirs = Vec::new();
    if with_reef {
        dirs.push(bin.to_path_buf());
    }
    for (n, dir) in std::env::split_paths(inherited).enumerate() {
        if dir == reef_bin_dir {
            continue;
        }
        if !dir.join("reef").exists() && !dir.join("reef-daemon").exists() {
            dirs.push(dir);
            continue;
        }
        let shadow = shadow_root.join(n.to_string());
        fs::create_dir_all(&shadow).map_err(|e| format!("{}: {e}", shadow.display()))?;
        let entries = fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name != "reef" && name != "reef-daemon" {
                std::os::unix::fs::symlink(entry.path(), shadow.join(&name))
                    .map_err(|e| format!("{}: {e}", shadow.display()))?;
            }
        }
        dirs.push(shadow);
    }
    std::env::join_paths(dirs).map_err(|e| format!("PATH: {e}"))
}

/// The `PATH` an agent gets: [`agent_path`] over the benchmark's own.
pub fn trial_path(
    with_reef: bool,
    bin: &Path,
    reef_bin_dir: &Path,
    shadow_root: &Path,
) -> Result<OsString, String> {
    let inherited = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    agent_path(&inherited, with_reef, bin, reef_bin_dir, shadow_root)
}

/// A trial's `~/.gitconfig`: an identity, as any developer machine has, and
/// quiet defaults.
pub fn write_gitconfig(home: &Path, name: &str, email: &str) -> Result<(), String> {
    fs::create_dir_all(home).map_err(|e| format!("{}: {e}", home.display()))?;
    fs::write(
        home.join(".gitconfig"),
        format!(
            "[user]\n\tname = {name}\n\temail = {email}\n[init]\n\tdefaultBranch = main\n[advice]\n\tdetachedHead = false\n"
        ),
    )
    .map_err(|e| format!("~/.gitconfig: {e}"))
}

/// Copies `reef` and `reef-daemon` from `reef_bin_dir` into `bin`.
pub fn copy_reef(reef_bin_dir: &Path, bin: &Path) -> Result<(), String> {
    fs::create_dir_all(bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    for binary in ["reef", "reef-daemon"] {
        fs::copy(reef_bin_dir.join(binary), bin.join(binary))
            .map_err(|e| format!("copying {binary}: {e}"))?;
    }
    Ok(())
}

/// A new directory under `work_root`, kept until the caller deletes it. Its
/// path stays short: a daemon's socket may live in it, and Unix sockets
/// have a 103-byte limit on macOS.
pub fn short_tempdir(work_root: &Path, prefix: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(work_root).map_err(|e| format!("{}: {e}", work_root.display()))?;
    Ok(tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(work_root)
        .map_err(|e| format!("trial directory: {e}"))?
        .keep())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_path_has_no_other_reef() {
        let tmp = tempfile::tempdir().unwrap();
        let [bin, build, tools, plain, shadows] =
            ["bin", "build", "tools", "plain", "shadows"].map(|name| tmp.path().join(name));
        for dir in [&bin, &build, &tools, &plain] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(build.join("reef"), "").unwrap();
        for name in ["reef", "reef-daemon", "git"] {
            fs::write(tools.join(name), "").unwrap();
        }
        let inherited = std::env::join_paths([&build, &tools, &plain]).unwrap();

        let path = agent_path(&inherited, true, &bin, &build, &shadows).unwrap();
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        // `tools` keeps its `git` through a shadow without `reef`.
        let shadow = shadows.join("1");
        assert_eq!(dirs, [bin.clone(), shadow.clone(), plain.clone()]);
        assert!(shadow.join("git").exists());
        assert!(!shadow.join("reef").exists() && !shadow.join("reef-daemon").exists());

        let path = agent_path(&inherited, false, &bin, &build, &tmp.path().join("s2")).unwrap();
        assert!(!std::env::split_paths(&path).any(|dir| dir == bin));
    }
}
