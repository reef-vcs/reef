# reef-client

The thin client of **Reef**, version control built for a world where AI agents write most of the code: everything a process needs to talk to the Reef daemon without being one ([ADR 0033](../../docs/adr/0033-thin-client.md)).

- `Client`: one method per `reef.api.v1` RPC over the daemon's Unix socket, with `*_all` helpers that walk every page, the `reef-agent` name an operation records (`Client::with_agent`), and detection of a daemon of another version.
- `json`: the stable JSON form of the responses that `reef -o json` prints.
- `paths`: where the daemon listens, settled exactly as the daemon settles it (`ClientConfig::load`).
- `error`: the kind, `ErrorDetail` and advice of an error status.
- `logging`: stderr logging from `REEF_LOG`, `RUST_LOG` and `REEF_LOG_FORMAT`.
- `backup`: a backup's `manifest.json`.

It depends on `reef-api` and `reef-error` and on no other Reef crate, and never on jj-lib: the `reef` binary (`reef-vcs`) is built on it. The daemon (`reef-daemon`) re-exports it as `reef_daemon::client`. The design document and roadmap are at <https://github.com/reef-vcs/reef>.
