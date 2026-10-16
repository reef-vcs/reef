# 0005. Tauri 2 for the desktop shell

- Status: accepted
- Date: 2026-10-15
- Ticket: R0.07

Backfilled from design.md; decision predates this record.

## Context

The human interface is one web application, rendered against the local daemon or a remote store ([Where it lives](../design.md#where-it-lives)). A desktop app adds what only a native app can: OS notifications for the needs-you-now bucket, a menu bar view of fleet state, a bundled auto-updating daemon, and deep links. The daemon is Rust. The same shell also builds for iOS and Android, with no bundled daemon (R3.40). The desktop shell ships only once the triage inbox has real proposals to show (R3.39), because a desktop app carries its own costs in signing, auto-update, and three platforms.

## Decision

We will build the desktop shell with Tauri 2. The mobile apps are the same shell built for iOS and Android, without the bundled daemon (R3.40).

- The Rust backend links or supervises the daemon.
- The UI is the same web application, rendered in the platform's native webview.
- Electron is the documented fallback if webview inconsistencies across platforms bite. The shell is kept thin so it stays cheap to swap.

## Consequences

- Small binaries, around 10 MB, and no second runtime alongside the Rust daemon *(addition: the runtime point)*.
- The UI must stay a plain web app, with no Tauri-specific code outside the shell, or the Electron fallback stops being cheap *(addition)*.
- Webview differences between platforms are a known risk that testing must cover.
- Signing and packaging for three desktop platforms and two app stores is a recurring cost (R3.41, R4.17).

## Alternatives considered

**Electron.** A consistent Chromium webview on every platform. Rejected for now: binaries around 150 MB and a second runtime alongside the Rust daemon *(addition: the size figure)*. It remains the fallback.

**Native apps per platform.** The best platform fit. Rejected: several codebases, and the UI would drift from the web UI the design requires the surfaces to share *(addition: the design does not name this alternative)*.

**No desktop app.** Rejected: loses OS notifications, the menu bar view, and the bundled daemon, which the triage inbox needs to reach a human promptly.
