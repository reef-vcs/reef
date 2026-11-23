//! Open at login (R3.39, ADR 0093): the person's choice, and what the app
//! does with it each time it launches or the choice changes.
//!
//! A login item names the app by its path, and the app can run from a path
//! that is gone by the next login: the mounted disk image, the copy macOS
//! translocates a quarantined app to, a development build. So the app keeps
//! the choice itself, writes the login item again for its current path on a
//! launch from a lasting place when it opens another, and shows the item as
//! on only when it opens this copy. A copy at a passing path offers no
//! toggle, since what it could write would not outlast it.

use std::path::{Component, Path};

/// The file in the app's data directory that holds the choice.
pub const CHOICE_FILE: &str = "open-at-login";

/// Whether the person wants the app opened at login: on until they turn it
/// off, since a notifier that is not running notifies nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Open the app at login.
    On,
    /// Do not.
    Off,
}

impl Choice {
    /// The choice saved at `file`; none, or one that does not read, is on.
    pub fn load(file: &Path) -> Choice {
        match std::fs::read_to_string(file) {
            Ok(saved) if saved.trim() == "off" => Choice::Off,
            _ => Choice::On,
        }
    }

    /// Saves the choice at `file`, whole or not at all: a torn "off" would
    /// read as on.
    pub fn save(self, file: &Path) -> std::io::Result<()> {
        let words = match self {
            Choice::On => "on\n",
            Choice::Off => "off\n",
        };
        let temporary = file.with_extension("tmp");
        std::fs::write(&temporary, words).and_then(|()| std::fs::rename(&temporary, file))
    }
}

/// What to do to the login item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    /// Write it for this app's path.
    Enable,
    /// Remove it.
    Disable,
    /// Leave it as it is.
    Leave,
}

/// What the menu shows for the login item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown {
    /// It opens this copy of the app.
    On,
    /// It does not.
    Off,
    /// This copy will not be at its path at the next login, so it offers no
    /// toggle.
    Elsewhere,
}

/// What the app does with `choice`: `exe` is its canonical path (none when
/// it does not resolve), `development` a debug build, which runs from a
/// build directory, and `agent` the launch agent's plist, if there is one.
/// Off removes the launch agent from any copy, one that does not read as
/// text included; on writes it only from a lasting path, and only when it
/// opens another.
pub fn act(choice: Choice, exe: Option<&Path>, development: bool, agent: Option<&str>) -> Act {
    match (choice, lasting(exe, development)) {
        (Choice::Off, _) => Act::Disable,
        (Choice::On, Some(exe)) if !opens(agent, exe) => Act::Enable,
        _ => Act::Leave,
    }
}

/// What the menu shows, `agent` read after [`act`].
pub fn shown(exe: Option<&Path>, development: bool, agent: Option<&str>) -> Shown {
    match lasting(exe, development) {
        None => Shown::Elsewhere,
        Some(exe) if opens(agent, exe) => Shown::On,
        Some(_) => Shown::Off,
    }
}

fn lasting(exe: Option<&Path>, development: bool) -> Option<&Path> {
    exe.filter(|exe| !development && !passing(exe))
}

/// A path macOS runs an app from only for now: a mounted volume (the `.dmg`)
/// or App Translocation's randomized copy of a quarantined app.
fn passing(exe: &Path) -> bool {
    exe.starts_with("/Volumes")
        || exe
            .components()
            .any(|part| part == Component::Normal("AppTranslocation".as_ref()))
}

/// Whether the launch agent `agent` (its plist, if there is one) opens the
/// app at `exe`: its program is `exe`, as the autostart plugin writes it.
fn opens(agent: Option<&str>, exe: &Path) -> bool {
    let program = format!("<string>{}</string>", exe.display());
    agent.is_some_and(|agent| agent.contains(&program))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSTALLED: &str = "/Applications/Reef.app/Contents/MacOS/reef-app";
    const PASSING: [&str; 3] = [
        "/Volumes/Reef/Reef.app/Contents/MacOS/reef-app",
        "/private/var/folders/x1/abc/T/AppTranslocation/0C1D-2E3F/d/Reef.app/Contents/MacOS/reef-app",
        "/Volumes",
    ];

    fn agent(program: &str) -> String {
        format!(
            "<plist version=\"1.0\"><dict><key>Label</key><string>Reef</string>\
             <key>ProgramArguments</key><array><string>{program}</string></array>\
             <key>RunAtLoad</key><true/></dict></plist>"
        )
    }

    #[test]
    fn the_choice_is_on_until_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(CHOICE_FILE);
        assert_eq!(Choice::load(&file), Choice::On, "a first launch");
        Choice::Off.save(&file).unwrap();
        assert_eq!(Choice::load(&file), Choice::Off);
        Choice::On.save(&file).unwrap();
        assert_eq!(Choice::load(&file), Choice::On);
        std::fs::write(&file, [0xff, 0xfe]).unwrap();
        assert_eq!(Choice::load(&file), Choice::On, "a file that does not read");
    }

    #[test]
    fn a_launch_from_a_lasting_place_points_the_item_at_itself() {
        let installed = Some(Path::new(INSTALLED));
        let dmg = agent(PASSING[0]);
        assert_eq!(
            act(Choice::On, installed, false, None),
            Act::Enable,
            "a first launch"
        );
        assert_eq!(
            act(Choice::On, installed, false, Some(&dmg)),
            Act::Enable,
            "a launch agent left at a vanished path"
        );
        let moved = Path::new("/Users/me/Applications/Reef.app/Contents/MacOS/reef-app");
        assert_eq!(
            act(Choice::On, Some(moved), false, Some(&agent(INSTALLED))),
            Act::Enable
        );
        assert_eq!(
            act(Choice::On, installed, false, Some(&agent(INSTALLED))),
            Act::Leave,
            "it already opens this copy"
        );
        let volumes_like = Path::new("/Volumesque/Reef.app/Contents/MacOS/reef-app");
        assert_eq!(
            act(Choice::On, Some(volumes_like), false, None),
            Act::Enable
        );
    }

    #[test]
    fn off_removes_the_item_from_any_copy() {
        let item = agent(INSTALLED);
        for exe in [Some(INSTALLED), Some(PASSING[0]), Some(PASSING[1]), None] {
            let exe = exe.map(Path::new);
            assert_eq!(
                act(Choice::Off, exe, false, Some(&item)),
                Act::Disable,
                "{exe:?}"
            );
            assert_eq!(
                act(Choice::Off, exe, false, None),
                Act::Disable,
                "{exe:?}: none, or one that does not read"
            );
        }
        assert_eq!(
            act(Choice::Off, Some(Path::new(INSTALLED)), true, Some(&item)),
            Act::Disable
        );
    }

    #[test]
    fn a_passing_copy_never_writes_the_item_and_offers_no_toggle() {
        let built = "/src/reef/target/shell/debug/bundle/macos/Reef.app/Contents/MacOS/reef-app";
        let cases = PASSING
            .iter()
            .map(|exe| (Some(Path::new(*exe)), false))
            .chain([(Some(Path::new(built)), true), (None, false)]);
        for (exe, development) in cases {
            assert_eq!(
                act(Choice::On, exe, development, None),
                Act::Leave,
                "{exe:?}"
            );
            let own = exe.map(|exe| agent(&exe.display().to_string()));
            assert_eq!(
                shown(exe, development, own.as_deref()),
                Shown::Elsewhere,
                "{exe:?}"
            );
        }
    }

    #[test]
    fn the_item_shows_on_only_when_it_opens_this_copy() {
        let exe = Some(Path::new(INSTALLED));
        assert_eq!(shown(exe, false, Some(&agent(INSTALLED))), Shown::On);
        assert_eq!(shown(exe, false, None), Shown::Off, "no launch agent");
        let dmg = agent(PASSING[0]);
        assert_eq!(
            shown(exe, false, Some(&dmg)),
            Shown::Off,
            "a launch agent at a vanished path"
        );
        let longer = agent("/Applications/Reef.app/Contents/MacOS/reef-app-old");
        assert_eq!(shown(exe, false, Some(&longer)), Shown::Off);
    }
}
