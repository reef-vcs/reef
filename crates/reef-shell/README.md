# reef-shell

The core of Reef's desktop app (R3.39): its connection to the daemon, which needs-you-now items it notifies, its menu bar and its `reef://` links. It has no UI toolkit; the Tauri app in `shell/` implements its `Platform` trait. See [docs/desktop.md](../../docs/desktop.md) and ADR 0093.
