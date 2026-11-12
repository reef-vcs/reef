//! The build-output spike's seeds (#264, experiments E1 to E3): before a
//! ticket's patch is applied, its workspace gets a copy-on-write clone of a
//! source workspace's build directories.
//!
//! The source is one more `reef ws run --keep` before the five tickets
//! start: built on the same base unedited (`sibling`), after ticket 1's
//! patch (`sibling-edited`), or built and then reduced to dependencies by
//! the repository's `seed_prepare` (`clone`, the shape a clone store would
//! have). The clone is `cp -c` on macOS (APFS `clonefile`) and
//! `cp --reflink=auto` on Linux (a copy on ext4, which cannot clone), with
//! timestamps preserved as a clone store's would be, so on APFS, btrfs and
//! XFS it costs no data blocks until a build rewrites them. No daemon
//! change is involved.

use std::fmt::Write as _;
use std::path::Path;

use super::SeedMode;

/// Quotes `text` for `sh`.
pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The shell lines that clone `dirs` from the source workspace `source`
/// into the current directory. A directory the source lacks is skipped.
pub fn clone_lines(source: &Path, dirs: &[String]) -> String {
    let copy = if cfg!(target_os = "macos") {
        "cp -c -R -p"
    } else {
        "cp -R -p --reflink=auto"
    };
    dirs.iter().fold(String::new(), |mut out, dir| {
        let from = quote(&source.join(dir).to_string_lossy());
        let to = quote(dir);
        let _ = writeln!(
            out,
            "if [ -d {from} ]; then mkdir -p \"$(dirname {to})\" && {copy} {from} {to}; fi"
        );
        out
    })
}

/// What the seed's source workspace runs, given the patch it applies first
/// (if any), the build, and the repository's `seed_prepare`. `None` for
/// [`SeedMode::Off`]; an error when `clone` has no `seed_prepare`.
pub fn source_script(
    mode: SeedMode,
    patch: &Path,
    build: &str,
    seed_prepare: Option<&str>,
) -> Result<Option<String>, String> {
    let mut script = String::new();
    match mode {
        SeedMode::Off => return Ok(None),
        SeedMode::Sibling | SeedMode::Clone => {}
        SeedMode::SiblingEdited => {
            let _ = writeln!(
                script,
                "git apply --whitespace=nowarn {}",
                quote(&patch.to_string_lossy())
            );
        }
    }
    // The build may fail (a test, say) and still leave its output: seeding
    // is about the output, so the source's exit status is not checked.
    let _ = writeln!(script, "( {build} ) || true");
    if mode == SeedMode::Clone {
        let prepare = seed_prepare
            .ok_or_else(|| "--seed clone needs the repository's `seed_prepare`".to_owned())?;
        script.push_str(prepare);
        script.push('\n');
    }
    Ok(Some(script))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_for_sh() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn clone_lines_clone_each_dir_that_exists() {
        let lines = clone_lines(
            Path::new("/ws/src"),
            &["target".to_owned(), "web/node_modules".to_owned()],
        );
        assert!(lines.contains("[ -d '/ws/src/target' ]"));
        assert!(lines.contains("'/ws/src/web/node_modules' 'web/node_modules'"));
        assert_eq!(lines.lines().count(), 2);
    }

    #[test]
    fn source_scripts_follow_the_mode() {
        let patch = Path::new("/p/1.patch");
        assert_eq!(
            source_script(SeedMode::Off, patch, "make", None).unwrap(),
            None
        );
        let sibling = source_script(SeedMode::Sibling, patch, "make", None)
            .unwrap()
            .unwrap();
        assert!(!sibling.contains("git apply"));
        let edited = source_script(SeedMode::SiblingEdited, patch, "make", None)
            .unwrap()
            .unwrap();
        assert!(edited.starts_with("git apply --whitespace=nowarn '/p/1.patch'"));
        assert!(source_script(SeedMode::Clone, patch, "make", None).is_err());
        let clone = source_script(SeedMode::Clone, patch, "make", Some("make clean-members"))
            .unwrap()
            .unwrap();
        assert!(clone.ends_with("make clean-members\n"));
    }
}
