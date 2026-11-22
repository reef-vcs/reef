# The desktop app

Reef.app puts the triage inbox ([inbox](inbox.md)) in your Mac's menu bar and tells you, with a banner, when something needs you: an agent asking for help, a proposal waiting on your sign-off, a landing that came back, a conflict nobody owns. It is for the person running the fleet. Agents never need it: everything it shows comes from the daemon, and `reef inbox watch` prints the same news for an agent supervising others. Its design is [ADR 0093](adr/0093-the-desktop-shell-and-raised-items.md). macOS only for now; Linux and Windows come with R3.41, and the phone with R3.40.

## Build it

```
just shell-build    # a debug Reef.app; prints its directory
just shell-run      # the same, opened on your daemon
```

It needs Xcode's command line tools and Node (the Tauri CLI runs through `npx`). `just shell-build` signs with `APPLE_SIGNING_IDENTITY`, else the first Apple Development identity in your keychain, else ad hoc. The first signing asks for your login keychain's permission ("codesign wants to sign using key"): choose **Always Allow**.

Sign with a team if you can. macOS keeps notification permission per signature: an ad-hoc app is held to its exact code, so after a rebuild macOS refuses its notifications without asking. An Apple Development or Developer ID signature stays the same across builds.

A release build (`scripts/shell-build.sh release <bin-dir>`) carries `reef` and `reef-daemon` inside the app and is signed ad hoc until Developer ID signing arrives (R4.17). macOS quarantines a downloaded `.dmg`: the first time, allow Reef in System Settings > Privacy & Security. There is no auto-update yet.

## The menu bar

The menu bar item shows how many things need you, and its menu lists the newest five, each opening its page. Below them: how many workspaces are active in how many repositories, landing pushes to a git remote that need a person, and the daemon's version. **Open Reef** opens the window; **Open at login** is on from the first launch, since a notifier that is not running notifies nothing; **Quit Reef** leaves the daemon and every agent running.

## Banners

A banner shows the daemon's sentence for the item ("Asks for help", "Needs a sign-off from release") and the repository's name, never an agent's own words. Clicking it opens the item.

- Each item is announced once. The daemon numbers each needs-you-now episode ([what is news](inbox.md#what-is-news)), and the app keeps how far it has read in `~/Library/Application Support/io.github.reef-vcs.reef/notices.json`, so restarting the app or the daemon announces nothing twice, and what arrived while the app was closed is announced when it starts.
- What already needed you when the app first saw a repository is not announced; the menu bar count shows it.
- More than three at once make one banner ("5 things need you").
- A proposal rewritten while it waits for your sign-off is announced again (the sign-off binds its tree), and so is a new ask on the same proposal, such as a landing that came back after your sign-off. A landing that comes back a second time on the same commit is not announced again.
- After the daemon restarts, nothing is announced again.
- A banner for something handled elsewhere (answered, signed off, landed) is taken out of Notification Center 30 seconds after it leaves.
- With notifications off for Reef, the menu says so and opens System Settings.

## The window

The window is the daemon's own web UI, opened on a session the app asks the daemon for. No token is printed or put on a command line. It shows only the daemon's own pages; any other link opens in your browser, and the page cannot call into the app. Closing the window frees its memory; the menu bar item stays.

A passkey ceremony (signing off, approving, adding a passkey) cannot run inside the app: macOS runs passkeys in an app's web view only for the app's own web domains. Those pages offer **Continue in your browser**, which opens the same page in your default browser on a session of its own, against the same daemon address the daemon checks. The window updates when the sign-off lands.

## Links

`reef://local/<page>` opens the window at a page of the web UI: `reef://local/` for the inbox, `reef://local/repos/<repo>/proposals/<change>` for a proposal, `reef://local/repos/<repo>/items/<key>` for an inbox item by its key. A link carries only a page of the daemon's own address: no host, query or token, and opening a page never acts by itself.

## How it finds the daemon

The app uses the daemon on your socket, whoever started it, when it serves API revision 46 or later. When none answers at launch, it starts one with `reef daemon start`, from an installed `reef` (Homebrew, `install.sh`, `cargo install`) first and its own otherwise, so it never moves your database ahead of the CLI you installed. The daemon runs on its own and outlives the app. A daemon that stops while the app runs is not started again by the app: the menu offers **Start the daemon**. A daemon older than the app needs is restarted from the menu only when no workspace is active.

The app reads `REEF_SOCKET`, `REEF_STATE_DIR` and `REEF_CONFIG` as the CLI does, but an app opened from the Finder or at login gets no shell environment, so it uses the default daemon. `just shell-run` passes yours.

## Cost

With the window closed, the app holds one `WatchInbox` stream and reads the daemon's status at each 15-second keepalive; it has no timer of its own. Measured on an M-series Mac (2026-11-22): a release build idle with no window has a 14 MB physical footprint (`vmmap`; `ps` reports about 76 MB of resident pages, most of them system frameworks shared with other apps) and used no measurable CPU over two minutes; a debug build used 0.08 s of CPU over ten minutes. The app is about 8 MB, plus `reef` and `reef-daemon` in a release build. Opening the window adds the web page's own streams and WebKit's processes, which closing it frees. The daemon serves the app's stream from the board every stream shares.

## Limits

- macOS 13 or later.
- No auto-update, and ad-hoc release builds, until R4.17.
- Banners from a build signed ad hoc stop after a rebuild or an update (see Build it).
- The app watches every registered repository; there is no per-repository mute. The system's Focus modes silence it.
